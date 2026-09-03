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
use std::time::{Duration, Instant, SystemTime};

use rszigbee_devices::{Definition, DefinitionIndex, Extend, PowerSourceHint};
use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, Ieee, Nwk};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zcl::registry::ClusterRegistry;
use rszigbee_spec::zdo::ZdoClusterId;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::behavior::BehaviorRegistry;
use super::inventory::{self, Inventory};
use super::{
    ConfigureOutcome, InterviewOutcome, InterviewUpdate, Request, RuntimeError, ZDO_TIMEOUT,
    Zigbee, interview,
};
use super::{decode, definitions, encode, tuya};
use crate::adapter::{AdapterError, AdapterEvent, CoordinatorAdapter, StartOutcome};
use crate::command::{CommandError, CommandOutcome, Confirmation, DeviceCommand};
use crate::device::{InterviewState, PowerSource};
use crate::event::{Event, LastSeenReason, LeaveReason};
use crate::reachability::{
    Evidence, NextCheck, ProbeResult, ReachabilityContext, ReachabilityPolicy,
};
use crate::store::{PersistedDevice, PersistedNetwork, ZigbeeStore};

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

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    async fn run(mut self, outcome: StartOutcome) {
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
                }
            }
        }
    }

    /// Records network identity the first time we see this coordinator.
    ///
    /// The network **key** is deliberately absent: it is not on
    /// [`NetworkInfo`], because a coordinator will not hand its key back. A
    /// store record after a resume can describe the network but not recreate
    /// it, which is what coordinator backups are for.
    async fn persist_network_if_needed(&mut self) {
        if self.network_known {
            return;
        }
        let Ok(info) = self.adapter.network_info().await else {
            debug!("no network parameters available to persist yet");
            return;
        };
        let record = PersistedNetwork {
            pan_id: info.pan_id,
            extended_pan_id: info.extended_pan_id,
            channel: info.channel,
            nwk_update_id: info.nwk_update_id,
            coordinator_ieee: self.coordinator,
            key_sequence: 0,
            frame_counter: 0,
        };
        if let Err(e) = self.store.save_network(&record).await {
            warn!(error = %e, "could not persist network identity");
        } else {
            self.network_known = true;
        }
    }

    fn emit(&self, event: Event) {
        // A send failure means nobody is subscribed. That is normal — a caller
        // using only `devices()` never subscribes — so it is not an error.
        let _ = self.events.send(event);
    }

    async fn shutdown(&mut self) -> Result<(), RuntimeError> {
        self.emit(Event::Stopping);
        // Fail the pending requests explicitly. Dropping their senders would
        // also wake the callers, but with `Stopped` rather than a reason.
        for (_, pending) in self.pending_zdo.drain() {
            let _ = pending.reply.send(Err(RuntimeError::Stopped));
        }
        for (_, pending) in self.pending_zcl.drain() {
            let _ = pending.reply.send(Err(RuntimeError::Stopped));
        }
        let flushed = self.store.flush().await;
        let stopped = self.adapter.stop().await;
        // Both are attempted before either error is returned: skipping the
        // flush because the adapter failed would lose state for no reason.
        stopped?;
        flushed?;
        Ok(())
    }

    // ---- requests

    async fn handle_request(&mut self, request: Request) {
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

    async fn permit_join(
        &mut self,
        duration: Duration,
        via: Option<Ieee>,
    ) -> Result<(), RuntimeError> {
        // A router is named by its permanent address at this layer and by its
        // short address at the adapter, so the translation happens here.
        let nwk = match via {
            Some(ieee) => Some(
                self.devices
                    .get(ieee)
                    .map(|e| e.info.nwk)
                    .ok_or(RuntimeError::UnknownDevice(ieee))?,
            ),
            None => None,
        };
        self.adapter.permit_join(duration, nwk).await?;
        let permitted = !duration.is_zero();
        self.emit(Event::PermitJoinChanged {
            permitted,
            until: permitted.then(|| Instant::now() + duration),
            via,
        });
        Ok(())
    }

    async fn send_zdo(
        &mut self,
        ieee: Ieee,
        cluster: ZdoClusterId,
        build: Box<dyn FnOnce(u8) -> Vec<u8> + Send>,
        reply: oneshot::Sender<Result<Vec<u8>, RuntimeError>>,
    ) {
        let Some(nwk) = self.devices.get(ieee).map(|e| e.info.nwk) else {
            let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
            return;
        };

        // Allocated here, immediately before the payload is built with it, so
        // the value that goes on the wire is the value we correlate on.
        self.zdo_sequence = self.zdo_sequence.wrapping_add(1);
        let sequence = self.zdo_sequence;
        let payload = build(sequence);

        let tx = crate::adapter::ZdoTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            cluster,
            payload,
            options: crate::adapter::TxOptions::default(),
        };

        match self.adapter.send_zdo(tx).await {
            // Some adapters answer inline; most deliver the response as an
            // event. Both are supported rather than one being assumed.
            Ok(Some(payload)) => {
                let _ = reply.send(Ok(payload));
            }
            Ok(None) => {
                self.pending_zdo.insert(
                    (cluster.response(), sequence),
                    PendingZdo {
                        reply,
                        ieee,
                        deadline: Instant::now() + ZDO_TIMEOUT,
                    },
                );
            }
            Err(e) => {
                let _ = reply.send(Err(e.into()));
            }
        }
    }

    /// Reads attributes, correlating the response by transaction sequence.
    async fn zcl_read(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: &[AttrId],
        reply: oneshot::Sender<Result<Vec<(u16, ZclValue)>, RuntimeError>>,
    ) {
        let Some(nwk) = self.devices.get(ieee).map(|e| e.info.nwk) else {
            let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
            return;
        };

        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let tsn = self.zcl_sequence;

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster,
            frame: encode::read_attributes(tsn, attributes),
            options: crate::adapter::TxOptions {
                expect_response: true,
                ..crate::adapter::TxOptions::default()
            },
        };

        match self.adapter.send_zcl(tx).await {
            // Some adapters answer inline; the Ember one delivers the response
            // as an event. Both are handled rather than one being assumed.
            Ok(Some(rx)) => {
                let decoded = decode::zcl_message(&self.registry, ieee, &rx)
                    .ok()
                    .and_then(|m| match m.kind {
                        crate::event::ZclMessageKind::Attributes(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                let _ = reply.send(Ok(decoded));
            }
            Ok(None) => {
                self.pending_zcl.insert(
                    (cluster, tsn),
                    PendingZcl {
                        reply,
                        ieee,
                        deadline: Instant::now() + ZDO_TIMEOUT,
                    },
                );
            }
            Err(e) => {
                let _ = reply.send(Err(e.into()));
            }
        }
    }

    /// Fails ZDO requests whose deadline passed or whose caller went away.
    ///
    /// Without this the map grows for every device that never answers, which
    /// is a slow leak in exactly the deployment where devices do not answer.
    fn expire_pending_zdo(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending_zdo
            .iter()
            .filter(|(_, p)| p.deadline <= now || p.reply.is_closed())
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            if let Some(pending) = self.pending_zdo.remove(&key) {
                let _ = pending.reply.send(Err(RuntimeError::ZdoTimeout {
                    ieee: pending.ieee,
                    timeout: ZDO_TIMEOUT,
                }));
            }
        }

        let stale: Vec<_> = self
            .pending_zcl
            .iter()
            .filter(|(_, p)| p.deadline <= now || p.reply.is_closed())
            .map(|(k, _)| *k)
            .collect();
        for key in stale {
            if let Some(pending) = self.pending_zcl.remove(&key) {
                let _ = pending.reply.send(Err(RuntimeError::ZdoTimeout {
                    ieee: pending.ieee,
                    timeout: ZDO_TIMEOUT,
                }));
            }
        }
    }

    async fn run_command(
        &mut self,
        ieee: Ieee,
        command: DeviceCommand,
    ) -> Result<CommandOutcome, CommandError> {
        // Copied out rather than held: planning a capability command needs
        // `&mut self` for the Tuya sequence counter, and a borrow of the
        // device table spanning that call would conflict.
        let Some((nwk, endpoints)) = self
            .devices
            .get(ieee)
            .map(|e| (e.info.nwk, e.info.endpoints.clone()))
        else {
            return Err(CommandError::UnknownDevice(ieee));
        };
        let started = Instant::now();

        // One counter for both ZCL and ZDO would be wrong: they are separate
        // sequence spaces on the wire.
        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let tsn = self.zcl_sequence;

        let (requested_endpoint, cluster, frame) =
            match command {
                DeviceCommand::Zcl(zcl) => {
                    let frame = encode::command(&self.registry, ieee, tsn, &zcl).map_err(|e| {
                        CommandError::InvalidValue {
                            capability: crate::capability::CapabilityId::from("zcl"),
                            value: e.to_string(),
                        }
                    })?;
                    (zcl.endpoint, zcl.cluster, frame)
                }
                DeviceCommand::ZclAttributes(write) => {
                    let frame = encode::attribute_write(&self.registry, ieee, tsn, &write)
                        .map_err(|e| CommandError::InvalidValue {
                            capability: crate::capability::CapabilityId::from("zcl-attributes"),
                            value: e.to_string(),
                        })?;
                    (write.endpoint, write.cluster, frame)
                }
                // Everything else is mapped from the device's definition.
                // There is deliberately no fallback: without one there is no
                // way to know which cluster a capability lives on, and a guess
                // that is right on most devices is silently wrong on the rest.
                ref other => self.plan_capability_command(ieee, other, tsn)?,
            };

        // No definition means no default endpoint to fall back on, so an
        // absent one resolves to the endpoint that actually hosts the cluster.
        let endpoint = match requested_endpoint {
            Some(id) => {
                if !endpoints.is_empty() && !endpoints.iter().any(|e| e.id == id) {
                    return Err(CommandError::UnknownEndpoint(id));
                }
                id
            }
            None => endpoints
                .iter()
                .find(|e| e.input_clusters.contains(&cluster))
                .map_or(EndpointId(1), |e| e.id),
        };
        let options = crate::adapter::TxOptions::default();

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster,
            frame,
            options,
        };

        let result = self.adapter.send_zcl(tx).await;
        let now = Instant::now();
        match result {
            Ok(response) => {
                self.record_tx(ieee, now, Ok(()));
                Ok(CommandOutcome {
                    // No definition means no converter, so there is nothing to
                    // report optimistically. Saying `None` is honest; inventing
                    // a state would be published as fact by the MQTT layer.
                    optimistic_state: None,
                    confirmed: if response.is_some() {
                        Confirmation::Acked
                    } else {
                        Confirmation::NoResponseRequested
                    },
                    latency: now.saturating_duration_since(started),
                })
            }
            Err(AdapterError::Tx(failure)) => {
                self.record_tx(ieee, now, Err(failure));
                self.emit(Event::CommandFailed {
                    ieee,
                    capability: None,
                    failure,
                });
                Err(CommandError::Delivery(failure))
            }
            Err(e) => {
                warn!(%ieee, error = %e, "command failed below the ZCL layer");
                Err(CommandError::Timeout(
                    now.saturating_duration_since(started),
                ))
            }
        }
    }

    // ---- adapter events

    async fn handle_adapter_event(&mut self, event: AdapterEvent) {
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

    async fn on_joined(&mut self, ieee: Option<Ieee>, nwk: Nwk) {
        // Without a permanent address there is nothing to key a record on. The
        // short address is not a stable identity: it is reassigned, so storing
        // a device under one would attribute a later device's traffic to it.
        let Some(ieee) = ieee.or_else(|| self.devices.resolve(nwk)) else {
            warn!(%nwk, "a device joined without a permanent address, so it cannot be recorded");
            return;
        };

        let now = SystemTime::now();
        let known = self.devices.get(ieee).is_some();
        if known {
            if let Some(from) = self.devices.set_nwk(ieee, nwk) {
                self.emit(Event::DeviceAddressChanged {
                    ieee,
                    from,
                    to: nwk,
                });
            }
            self.emit(Event::DeviceAnnounced { ieee });
        } else {
            self.devices.insert(inventory::new_entry(ieee, nwk, now));
            self.emit(Event::DeviceJoined { ieee, nwk });
        }

        self.touch(ieee, now, LastSeenReason::Announce);
        self.persist(ieee).await;

        let needs_interview = self
            .devices
            .get(ieee)
            .is_some_and(|e| !matches!(e.info.interview, InterviewState::Successful));
        if self.interview_on_join && needs_interview {
            self.spawn_interview(ieee, None);
        }
    }

    async fn on_left(&mut self, ieee: Option<Ieee>, nwk: Option<Nwk>) {
        let Some(ieee) = ieee.or_else(|| nwk.and_then(|n| self.devices.resolve(n))) else {
            warn!("a device left without an address the runtime could resolve");
            return;
        };
        self.devices.remove(ieee);
        if let Err(e) = self.store.delete_device(ieee).await {
            warn!(%ieee, error = %e, "could not remove the device from the store");
        }
        self.emit(Event::DeviceLeft {
            ieee,
            reason: LeaveReason::Unknown,
        });
    }

    fn on_zcl(&mut self, rx: crate::adapter::ZclRx) {
        // Everything downstream is keyed by permanent address, so a frame we
        // cannot attribute is a frame we cannot report. It is logged rather
        // than dropped silently, because the fix is a ZDO address lookup and
        // this is the evidence that one is needed.
        let Some(ieee) = rx.ieee.or_else(|| self.devices.resolve(rx.nwk)) else {
            debug!(
                nwk = rx.nwk.raw(),
                cluster = rx.cluster.0,
                "a frame arrived from a short address with no known device"
            );
            return;
        };

        // A frame from a known device whose short address moved is the cheapest
        // place to notice a rejoin: it arrives before any announce would.
        if let Some(from) = self.devices.set_nwk(ieee, rx.nwk) {
            self.emit(Event::DeviceAddressChanged {
                ieee,
                from,
                to: rx.nwk,
            });
        }
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.info.link_quality = rx.link_quality.or(entry.info.link_quality);
        }

        // A frame carrying the transaction sequence of an outstanding read is
        // that read's answer. Checked before the report path, so a read does
        // not also surface as an unsolicited attribute report.
        if let Ok(frame) = rszigbee_spec::zcl::frame::ZclFrame::decode(&rx.frame)
            && let Some(pending) = self.pending_zcl.remove(&(rx.cluster, frame.header.tsn))
        {
            let decoded = decode::zcl_message(&self.registry, ieee, &rx)
                .ok()
                .and_then(|m| match m.kind {
                    crate::event::ZclMessageKind::Attributes(a) => Some(a),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = pending.reply.send(Ok(decoded));
            self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
            return;
        }

        // Decoded, then reported. A frame that will not decode still moves
        // `last_seen` and still produces an event, because it is proof the
        // device is alive and it is the only evidence anyone has for adding
        // support for it.
        match decode::zcl_message(&self.registry, ieee, &rx) {
            Ok(message) => {
                self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
                // The sensor path's last link: an attribute report becomes
                // typed capability state, so a caller sees `temperature: 21.37`
                // and not a cluster id and an integer.
                match &message.kind {
                    crate::event::ZclMessageKind::Attributes(attributes) => {
                        self.publish_state(ieee, rx.endpoint, rx.cluster, attributes);
                    }
                    // A button press. Emitted as an action, never folded into
                    // state: it is momentary, and a state object that carries
                    // it has to have it excluded again on the way out.
                    crate::event::ZclMessageKind::Command { id, params, .. } => {
                        // Tuya reports arrive as cluster-specific commands on
                        // the manufacturer cluster rather than as attribute
                        // reports, which is why they need their own branch:
                        // nothing in the standard path would ever see them.
                        if rx.cluster == rszigbee_spec::tuya::CLUSTER
                            && rszigbee_spec::tuya::is_report(rszigbee_spec::ids::CommandId(*id))
                        {
                            self.publish_tuya(ieee, rx.endpoint, &rx.frame);
                        } else {
                            self.publish_action(ieee, rx.endpoint, rx.cluster, *id, params);
                        }
                    }
                    crate::event::ZclMessageKind::DefaultResponse { .. } => {}
                }
                self.emit(Event::ZclMessage(message));
            }
            Err(reason) => {
                self.touch(
                    ieee,
                    SystemTime::now(),
                    LastSeenReason::MessageWithoutPayload,
                );
                self.emit(Event::UnparsedFrame {
                    ieee,
                    endpoint: rx.endpoint,
                    cluster: rx.cluster,
                    raw: rx.frame,
                    reason,
                });
            }
        }
    }

    fn on_zdo(&mut self, cluster: ZdoClusterId, nwk: Nwk, payload: Vec<u8>) {
        // Correlate first: a waiting caller wants the payload whether or not
        // the sender is a device we have a record of.
        if let Some(&sequence) = payload.first()
            && let Some(pending) = self.pending_zdo.remove(&(cluster, sequence))
        {
            let _ = pending.reply.send(Ok(payload.clone()));
        }

        if let Some(ieee) = self.devices.resolve(nwk) {
            self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
        }

        self.emit(Event::ZdoResponse {
            nwk,
            cluster,
            payload,
        });
    }

    // ---- reachability

    /// Records that a frame arrived, and re-assesses reachability.
    fn touch(&mut self, ieee: Ieee, at: SystemTime, reason: LastSeenReason) {
        let Some(entry) = self.devices.get_mut(ieee) else {
            return;
        };
        entry.info.last_seen = Some(at);
        entry.reachability.record_traffic(at);
        self.emit(Event::LastSeenChanged { ieee, at, reason });
        self.assess(ieee, Evidence::Traffic);
    }

    fn record_tx(
        &mut self,
        ieee: Ieee,
        at: Instant,
        result: Result<(), crate::adapter::TxFailure>,
    ) {
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.record_tx(at, result);
        }
        self.assess(
            ieee,
            match result {
                Ok(()) => Evidence::CommandAcked,
                Err(failure) => Evidence::CommandFailed(failure),
            },
        );
    }

    /// Asks the policy for a verdict and emits a change only if there was one.
    fn assess(&mut self, ieee: Ieee, evidence: Evidence) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let assessment = self.reachability.assess(&ReachabilityContext {
            device: &entry.info,
            current: &entry.reachability,
            now: Instant::now(),
            wall_now: SystemTime::now(),
        });

        let previous = entry.reachability.state;
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.state = assessment.verdict;
            entry.next_check = assessment.next;
        }
        if previous != assessment.verdict {
            self.emit(Event::ReachabilityChanged {
                ieee,
                from: previous,
                to: assessment.verdict,
                evidence,
            });
        }
    }

    /// Runs whatever the policy scheduled and is now due.
    async fn run_due_reachability(&mut self) {
        let now = Instant::now();
        let due: Vec<(Ieee, NextCheck)> = self
            .devices
            .all()
            .filter_map(|e| match e.next_check {
                NextCheck::Probe { at, .. } if at <= now => Some((e.info.ieee, e.next_check)),
                NextCheck::Reassess { at } if at <= now => Some((e.info.ieee, e.next_check)),
                _ => None,
            })
            .collect();

        for (ieee, next) in due {
            match next {
                NextCheck::Probe { allow_recovery, .. } => {
                    self.probe(ieee, allow_recovery).await;
                }
                // No probe: the device sleeps and cannot answer on demand, but
                // its silence still eventually means something.
                NextCheck::Reassess { .. } => self.assess(ieee, Evidence::Elapsed),
                NextCheck::AwaitTraffic => {}
            }
        }
    }

    /// Probes a device by reading `genBasic.zclVersion`.
    ///
    /// Every Zigbee device implements this attribute, so a probe needs no
    /// definition and cannot be refused for being unsupported. What is being
    /// tested is whether the device answers at all, not what it answers.
    async fn probe(&mut self, ieee: Ieee, allow_recovery: bool) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let nwk = entry.info.nwk;
        let endpoint = entry.info.endpoints.first().map_or(EndpointId(1), |e| e.id);

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster: encode::PROBE_CLUSTER,
            frame: encode::probe(0),
            options: crate::adapter::TxOptions {
                expect_response: true,
                // Route repair is slow and noisy, and a probe is expected to
                // fail sometimes. The policy decides when it is worth it.
                disable_recovery: !allow_recovery,
                ..crate::adapter::TxOptions::default()
            },
        };

        let result = self.adapter.send_zcl(tx).await;
        let at = Instant::now();
        let outcome = match result {
            Ok(_) => ProbeResult::Answered,
            Err(AdapterError::Tx(failure)) => ProbeResult::Failed(failure),
            // Anything below the ZCL layer is not evidence about the device, so
            // it is reported as the closest thing that is: no acknowledgement.
            Err(_) => ProbeResult::Failed(crate::adapter::TxFailure::NoAck),
        };
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.record_probe(at, outcome);
        }
        self.assess(ieee, Evidence::Probe(outcome));
    }

    // ---- interview

    /// Turns reported attributes into a capability state delta.
    ///
    /// Only attributes the definition actually models produce state. An
    /// unmodelled attribute is left to [`Event::ZclMessage`], which still
    /// carries it: inventing a capability name would put junk into a caller's
    /// state, and dropping the frame would lose the only evidence anyone has
    /// for modelling it.
    ///
    /// The event carries the delta, never a merged snapshot. Publishing a
    /// snapshot is a compatibility behaviour that belongs to the MQTT layer.
    fn publish_state(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: &[(u16, ZclValue)],
    ) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };

        let mut changes = crate::state::StateChanges::new();
        for (attribute, value) in attributes {
            if let Some((capability, state)) =
                definitions::report_to_state(definition, cluster, *attribute, value)
            {
                changes.set(capability, state);
            }
        }
        if changes.is_empty() {
            return;
        }

        // The endpoint is reported only when the device has more than one:
        // on a single-endpoint device it is noise, and on a multi-endpoint one
        // it is the difference between two identical capabilities.
        let multi = self
            .devices
            .get(ieee)
            .is_some_and(|e| e.info.endpoints.len() > 1);
        self.emit(Event::StateChanged {
            ieee,
            endpoint: multi.then_some(endpoint),
            changes,
        });
    }

    /// Registers the manufacturer-specific clusters a device's definition
    /// declares.
    ///
    /// Must happen before any frame from such a cluster is decoded: without
    /// the registration its attributes have no known types, so the frame
    /// decodes to nothing usable and the device looks like it reports
    /// rubbish. Registered per device, because the same id means different
    /// things to different manufacturers.
    fn register_custom_clusters(&mut self, ieee: Ieee) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };
        let custom = definitions::custom_clusters(definition);
        if custom.is_empty() {
            return;
        }
        for def in custom {
            debug!(
                %ieee,
                cluster = def.id.0,
                name = %def.name,
                "registering a manufacturer-specific cluster for this device"
            );
            self.registry.insert_for_device(ieee, def);
        }
    }

    /// Applies definition metadata that overrides what the device reported.
    ///
    /// Currently one thing, and it matters: `forcePowerSource` exists because
    /// 26 upstream devices lie about how they are powered. A mains device that
    /// misreports as battery is never probed, so it is never noticed to have
    /// died; a battery device that misreports as mains is probed until its
    /// battery is flat. Both come from the same wrong byte.
    async fn apply_definition_metadata(&mut self, ieee: Ieee) {
        let forced = self.resolve(ieee).and_then(|d| {
            d.extend.iter().find_map(|e| match e {
                Extend::ForcePowerSource { source } => Some(*source),
                _ => None,
            })
        });
        let Some(source) = forced else {
            return;
        };

        let (power, sleepy) = match source {
            PowerSourceHint::Mains => (PowerSource::Mains, false),
            PowerSourceHint::Dc => (PowerSource::Dc, false),
            PowerSourceHint::Battery => (PowerSource::Battery, true),
            // `PowerSourceHint` is `#[non_exhaustive]`, so a newer devices
            // crate can name a source this build does not know. Leaving the
            // reported value alone is the safe answer: overriding it with a
            // guess is how a device stops being probed.
            _ => return,
        };
        if let Some(entry) = self.devices.get_mut(ieee) {
            if entry.info.power_source == power {
                return;
            }
            debug!(
                %ieee,
                reported = ?entry.info.power_source,
                forced = ?power,
                "definition overrides the reported power source"
            );
            entry.info.power_source = power;
            entry.reachability.is_sleepy = sleepy;
        }
        self.persist(ieee).await;
    }

    /// Lowers a capability command through the device's definition.
    ///
    /// The Tuya datapoint table is tried first, because a definition can
    /// declare both and the datapoint is the one the device actually acts on:
    /// a Tuya switch ignores `genOnOff` entirely.
    fn plan_capability_command(
        &mut self,
        ieee: Ieee,
        command: &DeviceCommand,
        tsn: u8,
    ) -> Result<(Option<EndpointId>, ClusterId, Vec<u8>), CommandError> {
        let definition = self.resolve(ieee).ok_or(CommandError::NoDefinition)?;

        // A behaviour is consulted only for what the table cannot say, so the
        // declarative lookup runs first.
        let behaviour_points = if tuya::command_to_datapoint(definition, command).is_none() {
            self.devices.get(ieee).and_then(|entry| {
                tuya::command_via_behavior(definition, command, &entry.info, &self.behaviors)
            })
        } else {
            None
        };
        if let Some(points) = behaviour_points {
            if points.is_empty() {
                // Claimed and refused: the behaviour decided the command was
                // invalid, and sending a partial write would leave the device
                // in a state nobody asked for.
                return Err(CommandError::InvalidValue {
                    capability: crate::capability::CapabilityId::from("behavior"),
                    value: "the device behaviour refused this command".into(),
                });
            }
            self.tuya_sequence = self.tuya_sequence.wrapping_add(1);
            let payload = rszigbee_spec::tuya::encode(self.tuya_sequence, &points);
            return Ok((
                Some(EndpointId(1)),
                rszigbee_spec::tuya::CLUSTER,
                encode::planned(tsn, rszigbee_spec::tuya::DATA_REQUEST, &payload),
            ));
        }

        if let Some(point) = tuya::command_to_datapoint(definition, command) {
            self.tuya_sequence = self.tuya_sequence.wrapping_add(1);
            let payload = rszigbee_spec::tuya::encode(self.tuya_sequence, &[point]);
            return Ok((
                Some(EndpointId(1)),
                rszigbee_spec::tuya::CLUSTER,
                encode::planned(tsn, rszigbee_spec::tuya::DATA_REQUEST, &payload),
            ));
        }

        let entry = self
            .devices
            .get(ieee)
            .ok_or(CommandError::UnknownDevice(ieee))?;
        let planned = definitions::plan_command(definition, &entry.info, command)?;
        Ok((
            Some(planned.endpoint),
            planned.cluster,
            encode::planned(tsn, planned.command, &planned.payload),
        ))
    }

    /// Decodes a Tuya datapoint report into capability state.
    ///
    /// Decoded from the raw frame rather than from the already-decoded
    /// parameters, because the datapoint list is a composite type the ZCL
    /// registry cannot describe — it is exactly the `1011` synthetic type the
    /// cluster table marks unencodable.
    fn publish_tuya(&mut self, ieee: Ieee, endpoint: EndpointId, frame: &[u8]) {
        let Ok(parsed) = rszigbee_spec::zcl::frame::ZclFrame::decode(frame) else {
            return;
        };
        let (_, datapoints) = match rszigbee_spec::tuya::decode(parsed.payload) {
            Ok(decoded) => decoded,
            Err(reason) => {
                // Surfaced rather than dropped: a malformed Tuya frame is
                // usually a datapoint nobody has modelled, and the bytes are
                // the only evidence for modelling it.
                debug!(%ieee, %reason, "a Tuya payload could not be decoded");
                return;
            }
        };
        if datapoints.is_empty() {
            return;
        }

        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let Some(definition) = self
            .definitions
            .resolve(&definitions::device_match(&entry.info))
        else {
            return;
        };
        let changes =
            tuya::datapoints_to_state(definition, &datapoints, &entry.info, &self.behaviors);
        if changes.is_empty() {
            // The device reported datapoints the table does not name. Worth
            // seeing, because it is the signal that the definition is
            // incomplete for this firmware revision.
            debug!(
                %ieee,
                datapoints = ?datapoints.iter().map(|d| d.dp).collect::<Vec<_>>(),
                "Tuya datapoints arrived that the definition does not name"
            );
            return;
        }

        let multi = self
            .devices
            .get(ieee)
            .is_some_and(|e| e.info.endpoints.len() > 1);
        self.emit(Event::StateChanged {
            ieee,
            endpoint: multi.then_some(endpoint),
            changes,
        });
    }

    /// Emits an action for a received cluster command, if one is named.
    fn publish_action(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        id: u8,
        params: &[(String, ZclValue)],
    ) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };
        let Some((capability, action)) =
            definitions::command_to_action(definition, cluster, id, params)
        else {
            return;
        };
        let multi = self
            .devices
            .get(ieee)
            .is_some_and(|e| e.info.endpoints.len() > 1);
        self.emit(Event::Action {
            ieee,
            endpoint: multi.then_some(endpoint),
            capability,
            action,
        });
    }

    /// Binds and configures reporting, so the device actually sends data.
    ///
    /// Runs after resolution, because the plan comes from the definition. Both
    /// halves are needed and in this order: a binding tells the device where
    /// to send reports, and configuring reporting tells it when. Configure
    /// without bind and the device generates reports with nowhere to send
    /// them; bind without configure and many devices report only on a poll.
    ///
    /// A failure on one step is logged and the rest continue. Devices refuse
    /// bindings and reporting configuration routinely -- some because they
    /// report unconditionally and consider it meaningless -- and abandoning
    /// the whole plan on the first refusal would leave later, working steps
    /// unconfigured.
    async fn execute_configure_plan(&mut self, ieee: Ieee) -> ConfigureOutcome {
        let Some(definition) = self.resolve(ieee) else {
            return ConfigureOutcome::default();
        };
        let Some(entry) = self.devices.get(ieee) else {
            return ConfigureOutcome::default();
        };
        let plan = definitions::configure_plan(definition, &entry.info);
        if plan.is_empty() {
            return ConfigureOutcome::default();
        }

        let device_nwk = entry.info.nwk;
        let coordinator = self.coordinator;
        let mut bound: std::collections::HashSet<(EndpointId, ClusterId)> =
            std::collections::HashSet::new();
        let mut outcome = ConfigureOutcome::default();

        for step in plan {
            // One binding per (endpoint, cluster), however many attributes on
            // it are being configured.
            if bound.insert((step.endpoint, step.cluster)) {
                match self
                    .bind(ieee, device_nwk, step.endpoint, step.cluster, coordinator)
                    .await
                {
                    Ok(()) => {
                        debug!(%ieee, cluster = step.cluster.0, "bound");
                        outcome.bound = outcome.bound.saturating_add(1);
                    }
                    Err(e) => {
                        warn!(%ieee, cluster = step.cluster.0, error = %e, "bind failed");
                        outcome.failed = outcome.failed.saturating_add(1);
                    }
                }
            }

            let Some(attribute) = step.attribute else {
                continue;
            };
            // The wire type decides whether a reportable change is sent at
            // all, so a wrong type produces a frame the device rejects. The
            // plan carries it; the registry is only a fallback, because it
            // does not know every cluster a definition can name.
            let ty = step.attribute_type.or_else(|| {
                self.registry
                    .attr(Some(ieee), step.cluster, attribute)
                    .map(|a| a.ty)
            });
            let Some(ty) = ty else {
                warn!(
                    %ieee,
                    cluster = step.cluster.0,
                    attribute = attribute.0,
                    "no wire type known, so reporting cannot be configured safely"
                );
                outcome.failed = outcome.failed.saturating_add(1);
                continue;
            };

            match self
                .configure_reporting(
                    ieee,
                    device_nwk,
                    step.endpoint,
                    step.cluster,
                    &[encode::ReportRecord {
                        attribute,
                        ty,
                        min_interval: step.min_interval,
                        max_interval: step.max_interval,
                        min_change: step.min_change,
                    }],
                )
                .await
            {
                Ok(()) => outcome.configured = outcome.configured.saturating_add(1),
                Err(e) => {
                    warn!(
                        %ieee,
                        cluster = step.cluster.0,
                        attribute = attribute.0,
                        error = %e,
                        "configuring reporting failed"
                    );
                    outcome.failed = outcome.failed.saturating_add(1);
                }
            }
        }

        info!(
            %ieee,
            bound = outcome.bound,
            configured = outcome.configured,
            failed = outcome.failed,
            "reporting configured"
        );
        outcome
    }

    /// Sends a `Bind_req` and waits for its response.
    async fn bind(
        &mut self,
        ieee: Ieee,
        nwk: Nwk,
        endpoint: EndpointId,
        cluster: ClusterId,
        coordinator: Ieee,
    ) -> Result<(), AdapterError> {
        self.zdo_sequence = self.zdo_sequence.wrapping_add(1);
        let sequence = self.zdo_sequence;
        let payload = rszigbee_spec::zdo::encode_bind_req(
            sequence,
            ieee,
            endpoint,
            cluster,
            coordinator,
            // The coordinator's own application endpoint, which is where the
            // Ember adapter registers its clusters.
            EndpointId(1),
        );
        self.adapter
            .send_zdo(crate::adapter::ZdoTx {
                dest: crate::adapter::Destination::Unicast { ieee, nwk },
                cluster: ZdoClusterId::BIND_REQ,
                payload,
                options: crate::adapter::TxOptions::default(),
            })
            .await
            .map(|_| ())
    }

    /// Sends a `configureReporting` command.
    async fn configure_reporting(
        &mut self,
        ieee: Ieee,
        nwk: Nwk,
        endpoint: EndpointId,
        cluster: ClusterId,
        records: &[encode::ReportRecord],
    ) -> Result<(), AdapterError> {
        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let frame = encode::configure_reporting(self.zcl_sequence, records)
            .map_err(|e| AdapterError::Transport(format!("cannot encode reporting config: {e}")))?;
        self.adapter
            .send_zcl(crate::adapter::ZclTx {
                dest: crate::adapter::Destination::Unicast { ieee, nwk },
                endpoint,
                source_endpoint: EndpointId(1),
                profile: rszigbee_spec::ids::ProfileId::HA,
                cluster,
                frame,
                options: crate::adapter::TxOptions::default(),
            })
            .await
            .map(|_| ())
    }

    /// Resolves the definition for a device from what the interview learned.
    ///
    /// Re-resolved rather than cached: resolution is a hash lookup plus a few
    /// comparisons, and a cache would have to be invalidated every time a
    /// device's facts changed — which is exactly when getting it wrong matters.
    fn resolve(&self, ieee: Ieee) -> Option<&Definition> {
        let entry = self.devices.get(ieee)?;
        self.definitions
            .resolve(&definitions::device_match(&entry.info))
    }

    /// Applies one interview update. The only place interview results are
    /// written, and it runs on the loop.
    async fn apply_interview_update(&mut self, ieee: Ieee, update: InterviewUpdate) {
        match update {
            InterviewUpdate::Started => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.interview = InterviewState::InProgress;
                }
                self.emit(Event::InterviewStarted { ieee });
                // Persisted immediately so a crash mid-interview resumes as
                // `InProgress` rather than looking like it never began.
                self.persist(ieee).await;
            }
            InterviewUpdate::NodeDescriptor {
                kind,
                power,
                sleepy,
            } => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.kind = kind;
                    entry.info.power_source = power;
                    // The one fact that decides whether this device can ever be
                    // probed on demand. Getting it wrong means either probing a
                    // sleeping device forever or never noticing a dead one.
                    entry.reachability.is_sleepy = sleepy;
                }
                self.persist(ieee).await;
            }
            InterviewUpdate::Step(step) => {
                self.emit(Event::InterviewProgress { ieee, step });
            }
            InterviewUpdate::Finished(outcome) => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.interview = outcome.state;
                    if !outcome.endpoints.is_empty() {
                        entry.info.endpoints.clone_from(&outcome.endpoints);
                        entry.info.endpoints.sort_by_key(|e| e.id);
                    }
                    // The model string, which is what a definition matches on.
                    if let Some(basic) = &outcome.basic {
                        entry.info.basic = basic.clone();
                    }
                }
                self.persist(ieee).await;
                self.emit(Event::InterviewFinished {
                    ieee,
                    state: outcome.state,
                });

                // Resolution happens here rather than mid-interview because it
                // needs the model *and* the endpoints: some fingerprints match
                // on the endpoint layout.
                let resolved = self
                    .resolve(ieee)
                    .map(|d| (d.model.clone(), d.is_complete()));
                if let Some((model, complete)) = &resolved
                    && !complete
                {
                    debug!(
                        %ieee,
                        model,
                        "definition matched but is incomplete: some behaviour is not expressed"
                    );
                }
                let matched = resolved.is_some();
                // Emitted either way. An unrecognised device still produces raw
                // events, and this is the signal that what it needs is a
                // definition.
                self.emit(Event::DefinitionResolved {
                    ieee,
                    model: resolved.map(|(model, _)| model),
                    source: crate::event::DefinitionSource::Bundled,
                });

                if matched {
                    // Before anything else: a frame from a custom cluster
                    // cannot be decoded until its types are known.
                    self.register_custom_clusters(ieee);
                    self.apply_definition_metadata(ieee).await;
                }

                // Resolving a definition is only half of making a sensor work.
                // The other half is binding and configuring reporting, without
                // which the device is recognised and permanently silent.
                if matched {
                    let _ = self.execute_configure_plan(ieee).await;
                }
            }
        }
    }

    fn spawn_interview(
        &mut self,
        ieee: Ieee,
        reply: Option<oneshot::Sender<Result<InterviewOutcome, RuntimeError>>>,
    ) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let result = interview::run(&handle, ieee).await;
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        });
    }

    // ---- persistence

    async fn persist(&mut self, ieee: Ieee) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let record = inventory::persisted_from_entry(entry);
        if let Err(e) = self.store.upsert_device(&record).await {
            // Not fatal: the device is still usable this run, and failing the
            // whole runtime because one write failed would be worse than
            // continuing with a warning.
            warn!(%ieee, error = %e, "could not persist the device");
        }
    }
}
