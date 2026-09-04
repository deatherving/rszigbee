//! The runtime task: the single owner of the adapter.
//!
//! # The shape of the loop
//!
//! One `select!` over three sources — requests from handles, events from the
//! adapter, and a timer for scheduled reachability work. Everything that
//! touches the adapter happens here, in arrival order.
//!
//! # Why interviews run outside the loop
//!
//! An interview is a sequence of ZDO round trips, and a ZDO response arrives
//! *as an adapter event* — through this same loop. Awaiting an interview inside
//! the loop would therefore deadlock: the loop would be blocked waiting for a
//! response only the loop can deliver.
//!
//! So an interview runs as its own task holding a [`Zigbee`] handle, and reaches
//! the adapter the same way any other caller does. The loop stays responsive,
//! and a slow or unresponsive device cannot stall the runtime — which is worth
//! having, because a device that has to be waited out is the normal case, not
//! the exception.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rszigbee_devices::DefinitionIndex;
use rszigbee_spec::ids::{ClusterId, Ieee};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zcl::registry::ClusterRegistry;
use rszigbee_spec::zdo::ZdoClusterId;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::behavior::BehaviorRegistry;
use super::definitions;
use super::inventory::{self, Inventory};
use super::{Request, RuntimeError, Zigbee};
use crate::adapter::{AdapterEvent, CoordinatorAdapter, StartOutcome};
use crate::event::Event;
use crate::reachability::ReachabilityPolicy;
use crate::store::{PersistedDevice, ZigbeeStore};

/// Everything the task needs to run, assembled by the builder.
pub struct Config<A, S> {
    pub adapter: A,
    pub store: S,
    pub adapter_events: mpsc::Receiver<AdapterEvent>,
    pub requests: mpsc::Receiver<Request>,
    pub events: broadcast::Sender<Event>,
    pub reachability: Arc<dyn ReachabilityPolicy>,
    pub interview_on_join: bool,
    pub devices: Vec<PersistedDevice>,
    pub outcome: StartOutcome,
    pub coordinator: Ieee,
    pub network_known: bool,
    pub registry: ClusterRegistry,
    pub behaviors: BehaviorRegistry,
    pub definitions: DefinitionIndex,
}

/// A ZDO request awaiting its response.
struct PendingZdo {
    reply: oneshot::Sender<Result<Vec<u8>, RuntimeError>>,
    ieee: Ieee,
    deadline: Instant,
}

/// A ZCL read awaiting its response.
struct PendingZcl {
    reply: oneshot::Sender<Result<Vec<(u16, ZclValue)>, RuntimeError>>,
    ieee: Ieee,
    deadline: Instant,
}

/// Spawns the runtime task.
pub fn spawn<A: CoordinatorAdapter, S: ZigbeeStore>(config: Config<A, S>, handle: Zigbee) {
    let mut devices = Inventory::new();
    for stored in config.devices {
        devices.insert(inventory::entry_from_persisted(stored));
    }

    let task = Task {
        adapter: config.adapter,
        store: config.store,
        adapter_events: config.adapter_events,
        requests: config.requests,
        events: config.events,
        reachability: config.reachability,
        interview_on_join: config.interview_on_join,
        coordinator: config.coordinator,
        network_known: config.network_known,
        // Zero until a record is written. Also correct as a starting value if a
        // network was already stored: the first refresh reads the live counter
        // and tops the margin up, which is cheap and cannot roll anything back.
        persisted_frame_counter: 0,
        devices,
        pending_zdo: HashMap::new(),
        pending_zcl: HashMap::new(),
        zdo_sequence: 0,
        zcl_sequence: 0,
        tuya_sequence: 0,
        registry: config.registry,
        behaviors: config.behaviors,
        definitions: config.definitions,
        handle,
    };
    tokio::spawn(task.run(config.outcome));
}

struct Task<A, S> {
    adapter: A,
    store: S,
    adapter_events: mpsc::Receiver<AdapterEvent>,
    requests: mpsc::Receiver<Request>,
    events: broadcast::Sender<Event>,
    reachability: Arc<dyn ReachabilityPolicy>,
    interview_on_join: bool,
    coordinator: Ieee,
    network_known: bool,
    /// The frame counter value currently on disk, including its margin.
    ///
    /// Tracked so the periodic refresh is a comparison rather than a store
    /// read, and so a write happens only when the margin has actually been
    /// consumed.
    persisted_frame_counter: u32,
    devices: Inventory,
    pending_zdo: HashMap<(ZdoClusterId, u8), PendingZdo>,
    pending_zcl: HashMap<(ClusterId, u8), PendingZcl>,
    zdo_sequence: u8,
    zcl_sequence: u8,
    /// Tuya's own sequence space, separate from ZCL's: the datapoint payload
    /// carries a sequence of its own and devices track it independently.
    tuya_sequence: u16,
    registry: ClusterRegistry,
    behaviors: BehaviorRegistry,
    definitions: DefinitionIndex,
    handle: Zigbee,
}

mod availability;
mod commands;
mod devices;
mod inbound;
mod provisioning;
mod transport;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    pub(super) async fn run(mut self, outcome: StartOutcome) {
        self.register_coordinator();

        // Devices restored from the store were resolved before this process
        // existed, so their custom clusters have to be registered again or
        // their frames decode to nothing until they are re-interviewed.
        let known: Vec<Ieee> = self.devices.all().map(|e| e.info.ieee).collect();
        for ieee in known {
            self.register_custom_clusters(ieee);
        }

        self.persist_network_if_needed().await;
        self.emit(Event::Started { outcome });

        // A short tick rather than a computed sleep. Reachability deadlines
        // move whenever a frame arrives, and recomputing the next wake-up on
        // every event costs more complexity than it saves in wake-ups.
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                request = self.requests.recv() => match request {
                    Some(Request::Stop(reply)) => {
                        let result = self.shutdown().await;
                        let _ = reply.send(result);
                        return;
                    }
                    Some(request) => self.handle_request(request).await,
                    // Every handle was dropped. Shut the coordinator down
                    // rather than leaving a radio running with nobody
                    // listening.
                    None => {
                        info!("all runtime handles dropped, shutting down");
                        let _ = self.shutdown().await;
                        return;
                    }
                },
                event = self.adapter_events.recv() => {
                    if let Some(event) = event {
                        self.handle_adapter_event(event).await;
                    } else {
                        // The adapter's own task is gone, so there is no radio
                        // behind this runtime any more.
                        warn!("the adapter event channel closed");
                        let _ = self.shutdown().await;
                        return;
                    }
                },
                _ = ticker.tick() => {
                    self.expire_pending_zdo();
                    self.run_due_reachability().await;
                    self.refresh_frame_counter().await;
                }
            }
        }
    }

    pub(super) fn emit(&self, event: Event) {
        // A send failure means nobody is subscribed. That is normal — a caller
        // using only `devices()` never subscribes — so it is not an error.
        let _ = self.events.send(event);
    }

    pub(super) async fn shutdown(&mut self) -> Result<(), RuntimeError> {
        self.emit(Event::Stopping);
        // Fail the pending requests explicitly. Dropping their senders would
        // also wake the callers, but with `Stopped` rather than a reason.
        for (_, pending) in self.pending_zdo.drain() {
            let _ = pending.reply.send(Err(RuntimeError::Stopped));
        }
        for (_, pending) in self.pending_zcl.drain() {
            let _ = pending.reply.send(Err(RuntimeError::Stopped));
        }
        // Last chance to record where the counter actually got to. A clean
        // stop should not leave the stored value a thousand frames stale.
        self.refresh_frame_counter().await;
        let flushed = self.store.flush().await;
        let stopped = self.adapter.stop().await;
        // Both are attempted before either error is returned: skipping the
        // flush because the adapter failed would lose state for no reason.
        stopped?;
        flushed?;
        Ok(())
    }

    pub(super) async fn handle_request(&mut self, request: Request) {
        match request {
            Request::Devices(reply) => {
                let _ = reply.send(self.devices.snapshot());
            }
            Request::Device(ieee, reply) => {
                let _ = reply.send(self.devices.get(ieee).map(|e| e.info.clone()));
            }
            Request::Network(reply) => {
                let _ = reply.send(self.adapter.network_info().await);
            }
            Request::PermitJoin {
                duration,
                via,
                reply,
            } => {
                let _ = reply.send(self.permit_join(duration, via).await);
            }
            Request::ZclRead {
                ieee,
                endpoint,
                cluster,
                attributes,
                reply,
            } => {
                self.zcl_read(ieee, endpoint, cluster, &attributes, reply)
                    .await;
            }
            Request::Zdo {
                ieee,
                cluster,
                build,
                reply,
            } => self.send_zdo(ieee, cluster, build, reply).await,
            Request::Command {
                ieee,
                command,
                reply,
            } => {
                let _ = reply.send(self.run_command(ieee, *command).await);
            }
            Request::Interview { ieee, reply } => {
                if self.devices.get(ieee).is_none() {
                    let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
                    return;
                }
                self.spawn_interview(ieee, Some(reply));
            }
            Request::InterviewUpdate { ieee, update } => {
                self.apply_interview_update(ieee, *update).await;
            }
            Request::Configure(ieee, reply) => {
                if self.devices.get(ieee).is_none() {
                    let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
                    return;
                }
                let outcome = self.execute_configure_plan(ieee).await;
                let _ = reply.send(Ok(outcome));
            }
            Request::Definition(ieee, reply) => {
                let _ = reply.send(
                    self.resolve(ieee)
                        .map(|d| (d.model.clone(), d.is_complete())),
                );
            }
            Request::ConfigurePlan(ieee, reply) => {
                let plan = match (self.resolve(ieee), self.devices.get(ieee)) {
                    (Some(definition), Some(entry)) => {
                        definitions::configure_plan(definition, &entry.info)
                    }
                    // No definition means nothing is known to configure, which
                    // is an empty plan rather than an error.
                    _ => Vec::new(),
                };
                let _ = reply.send(plan);
            }
            // Handled in `run` so it can return.
            Request::Stop(reply) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    pub(super) async fn handle_adapter_event(&mut self, event: AdapterEvent) {
        match event {
            AdapterEvent::DeviceJoined { ieee, nwk } => self.on_joined(ieee, nwk).await,
            AdapterEvent::DeviceLeft { ieee, nwk } => self.on_left(ieee, nwk).await,
            AdapterEvent::Zcl(rx) => self.on_zcl(rx),
            AdapterEvent::Zdo {
                cluster,
                nwk,
                payload,
            } => self.on_zdo(cluster, nwk, payload),
            AdapterEvent::Disconnected(reason) => {
                warn!(?reason, "the coordinator link went away");
                self.emit(Event::AdapterDisconnected { reason });
            }
            // `AdapterEvent` is `#[non_exhaustive]` so a new variant is not a
            // breaking change. Ignoring one is better than refusing to build
            // against a newer adapter crate.
            other => debug!(?other, "an adapter event this runtime does not handle"),
        }
    }
}
