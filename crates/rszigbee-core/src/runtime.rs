//! The runtime: one task owning the adapter, and a cloneable handle onto it.
//!
//! This is what makes rszigbee a library rather than a set of types. Everything
//! below it — codecs, the adapter trait, the store — is driven from here, and
//! nothing here knows what coordinator family or transport is underneath.
//!
//! ```no_run
//! use rszigbee_core::runtime::Zigbee;
//! use rszigbee_core::store::MemoryStore;
//! use rszigbee_core::adapter::MockAdapter;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let (adapter, _control, events) = MockAdapter::new();
//! let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
//!     .start()
//!     .await?;
//!
//! println!("coordinator {}", zigbee.coordinator());
//! let mut stream = zigbee.events();
//! while let Some(event) = stream.recv().await {
//!     println!("{event:?}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Why one task owns the adapter
//!
//! [`CoordinatorAdapter`] takes `&mut self`, because a coordinator is one
//! serial port with one framing state machine and concurrent use is a protocol
//! violation, not a performance question. Rather than wrap it in a lock — which
//! makes every caller's ordering an accident of scheduling — exactly one task
//! owns it and everything else asks that task over a channel.
//!
//! The consequence worth knowing: [`Zigbee`] is cheap to clone and safe to use
//! from as many tasks as you like, and requests are served in the order they
//! arrive.
//!
//! # What the runtime does not do yet
//!
//! Capability-level commands — `SetOn`, `SetBrightness`, and the rest of
//! [`DeviceCommand`] — need a device definition to know which cluster and
//! attribute a capability maps to, and the definition engine does not exist.
//! They return [`CommandError::NoDefinition`] naming that, rather than
//! guessing a mapping. [`DeviceCommand::Zcl`] and
//! [`DeviceCommand::ZclAttributes`] work today: the runtime encodes them from
//! the cluster registry, so they need no device definition.

mod decode;
mod definitions;
mod encode;
mod interview;
mod inventory;
mod task;

use std::sync::Arc;
use std::time::Duration;

use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, Ieee};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zdo::ZdoClusterId;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::adapter::{
    AdapterError, AdapterEvent, CoordinatorAdapter, MismatchPolicy, NetworkConfig, StartOutcome,
};
use crate::command::{CommandError, CommandOutcome, DeviceCommand};
use crate::device::DeviceInfo;
use crate::event::Event;
use crate::reachability::{ReachabilityPolicy, SilencePolicy};
use crate::store::{StoreError, ZigbeeStore};

pub use definitions::{ConfigureStep, PlannedZcl};
pub use encode::EncodeError;
pub use interview::InterviewOutcome;
pub(crate) use interview::InterviewUpdate;

/// The channel used when none is given. 11 is the low end of the 2.4GHz
/// Zigbee channels and the most common default across the ecosystem.
const DEFAULT_CHANNEL: u8 = 11;

/// How long a ZDO request waits for its response before giving up.
const ZDO_TIMEOUT: Duration = Duration::from_secs(5);

/// Default capacity of the event channel.
///
/// Bounded on purpose: an unbounded event channel converts a slow consumer into
/// unbounded memory growth. Overflow surfaces as [`Event::Lagged`], because a
/// consumer that fell behind should see a visible gap rather than a silent one.
const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// Why a runtime operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// The runtime task is gone, so nothing can be served.
    #[error("the runtime has stopped")]
    Stopped,

    /// The coordinator or its transport failed.
    #[error("adapter: {0}")]
    Adapter(#[from] AdapterError),

    /// Persistence failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),

    /// No device with that address is known.
    #[error("no device with address {0}")]
    UnknownDevice(Ieee),

    /// A device did not answer a ZDO request in time.
    #[error("no ZDO response from {ieee} within {timeout:?}")]
    ZdoTimeout {
        /// Which device.
        ieee: Ieee,
        /// How long was allowed.
        timeout: Duration,
    },

    /// The store describes a network formed by a different coordinator.
    ///
    /// Refused rather than resolved: every joined device's link key was derived
    /// against the old coordinator's address, so continuing means a network
    /// full of devices that can no longer be talked to, and a store that has
    /// been overwritten with the wrong identity.
    #[error(
        "this coordinator is {found}, but the store holds a network formed by {expected}. \
         Either restore the original coordinator, or start against an empty data \
         directory and re-pair the devices."
    )]
    CoordinatorMismatch {
        /// The coordinator the store was written by.
        expected: Ieee,
        /// The coordinator actually attached.
        found: Ieee,
    },
}

/// A request to the runtime task.
enum Request {
    Devices(oneshot::Sender<Vec<DeviceInfo>>),
    Device(Ieee, oneshot::Sender<Option<DeviceInfo>>),
    Network(oneshot::Sender<Result<crate::adapter::NetworkInfo, AdapterError>>),
    Definition(Ieee, oneshot::Sender<Option<(String, bool)>>),
    ConfigurePlan(Ieee, oneshot::Sender<Vec<ConfigureStep>>),
    PermitJoin {
        duration: Duration,
        via: Option<Ieee>,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    /// A ZCL read. Correlated on the transaction sequence the task allocates,
    /// for the same reason ZDO is: the value on the wire has to be the value
    /// waited on.
    ZclRead {
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: Vec<AttrId>,
        reply: oneshot::Sender<Result<Vec<(u16, ZclValue)>, RuntimeError>>,
    },
    /// A ZDO request. The task allocates the sequence number and builds the
    /// payload with it, so sequence allocation and response correlation cannot
    /// drift apart.
    Zdo {
        ieee: Ieee,
        cluster: ZdoClusterId,
        build: Box<dyn FnOnce(u8) -> Vec<u8> + Send>,
        reply: oneshot::Sender<Result<Vec<u8>, RuntimeError>>,
    },
    Command {
        ieee: Ieee,
        command: Box<DeviceCommand>,
        reply: oneshot::Sender<Result<CommandOutcome, CommandError>>,
    },
    Interview {
        ieee: Ieee,
        reply: oneshot::Sender<Result<InterviewOutcome, RuntimeError>>,
    },
    /// Progress from an interview task, which runs off the loop and therefore
    /// cannot touch the device table directly.
    InterviewUpdate {
        ieee: Ieee,
        update: Box<InterviewUpdate>,
    },
    Stop(oneshot::Sender<Result<(), RuntimeError>>),
}

/// A subscription to the runtime's event stream.
///
/// Each stream is independent: two consumers both see every event, and one
/// falling behind does not slow the other. Falling behind is reported as
/// [`Event::Lagged`] rather than hidden.
#[derive(Debug)]
pub struct EventStream {
    inner: broadcast::Receiver<Event>,
}

impl EventStream {
    /// Waits for the next event, or `None` once the runtime has stopped and the
    /// buffered events have been drained.
    pub async fn recv(&mut self) -> Option<Event> {
        match self.inner.recv().await {
            Ok(event) => Some(event),
            // Surfaced, not swallowed. A gap in a timeline that nobody is told
            // about is the kind of bug that gets diagnosed as a device fault.
            Err(broadcast::error::RecvError::Lagged(skipped)) => Some(Event::Lagged { skipped }),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

/// Builds a [`Zigbee`] runtime.
///
/// The adapter and the store are type parameters rather than boxed traits: an
/// embedded caller gets static dispatch and no allocation, and the concrete
/// adapter type stays visible so `EmberAdapter`-specific methods remain
/// reachable before the runtime takes ownership.
pub struct ZigbeeBuilder<A, S> {
    adapter: A,
    adapter_events: mpsc::Receiver<AdapterEvent>,
    store: S,
    network: NetworkConfig,
    backup: Option<Vec<u8>>,
    reachability: Arc<dyn ReachabilityPolicy>,
    event_capacity: usize,
    interview_on_join: bool,
    registry: rszigbee_spec::zcl::registry::ClusterRegistry,
    definitions: rszigbee_devices::DefinitionIndex,
}

impl<A, S> core::fmt::Debug for ZigbeeBuilder<A, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ZigbeeBuilder")
            .field("event_capacity", &self.event_capacity)
            .field("interview_on_join", &self.interview_on_join)
            .finish_non_exhaustive()
    }
}

impl<A: CoordinatorAdapter, S: ZigbeeStore> ZigbeeBuilder<A, S> {
    /// The network to join or form.
    ///
    /// The default refuses to form one ([`MismatchPolicy::Fail`]), so a
    /// coordinator whose network does not match is an error rather than a
    /// silently re-formed network.
    #[must_use]
    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.network = network;
        self
    }

    /// A coordinator backup to restore while starting.
    #[must_use]
    pub fn restore(mut self, backup: Vec<u8>) -> Self {
        self.backup = Some(backup);
        self
    }

    /// The availability policy. Defaults to [`SilencePolicy`].
    ///
    /// This is the seam that keeps availability out of the MQTT layer: an
    /// embedded application gets reachability without running a broker, and a
    /// deployment with different ideas about when to probe replaces the policy
    /// instead of patching the runtime.
    #[must_use]
    pub fn reachability_policy(mut self, policy: impl ReachabilityPolicy + 'static) -> Self {
        self.reachability = Arc::new(policy);
        self
    }

    /// Capacity of the event channel. Overflow becomes [`Event::Lagged`].
    #[must_use]
    pub fn event_capacity(mut self, events: usize) -> Self {
        self.event_capacity = events.max(1);
        self
    }

    /// The device definitions used to recognise devices and map commands.
    ///
    /// Empty by default, which means capability commands are refused: without
    /// a definition there is no way to know which cluster `SetOn` belongs to,
    /// and guessing is how a command lands on the wrong cluster and appears to
    /// do nothing.
    #[must_use]
    pub fn definitions(mut self, definitions: rszigbee_devices::DefinitionIndex) -> Self {
        self.definitions = definitions;
        self
    }

    /// The cluster registry used to type ZCL parameters and attributes.
    ///
    /// Defaults to the built-in set. Replace it to add a manufacturer-specific
    /// cluster, which is what makes a device with a custom cluster addressable
    /// through the ZCL escape hatch before it has a definition.
    #[must_use]
    pub fn registry(mut self, registry: rszigbee_spec::zcl::registry::ClusterRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Whether a newly joined device is interviewed automatically.
    ///
    /// On by default. Turning it off leaves [`Zigbee::interview`] to be called
    /// explicitly, which is what a caller wants when it needs to control when
    /// the radio is busy.
    #[must_use]
    pub fn interview_on_join(mut self, yes: bool) -> Self {
        self.interview_on_join = yes;
        self
    }

    /// Starts the coordinator and the runtime task.
    ///
    /// # Errors
    ///
    /// Fails if the coordinator will not start, if the store cannot be read, or
    /// if the store describes a network formed by a different coordinator.
    pub async fn start(mut self) -> Result<Zigbee, RuntimeError> {
        let outcome = self
            .adapter
            .start(&self.network, self.backup.as_deref())
            .await?;
        let coordinator = self.adapter.coordinator_ieee().await?;

        // Before anything is written: does this store belong to this
        // coordinator? Getting this wrong is unrecoverable, so it is checked
        // once, early, and refused rather than reconciled.
        let stored = self.store.load_network().await?;
        if let Some(network) = &stored
            && network.coordinator_ieee != coordinator
        {
            return Err(RuntimeError::CoordinatorMismatch {
                expected: network.coordinator_ieee,
                found: coordinator,
            });
        }

        let devices = self.store.load_devices().await?;
        tracing::info!(
            coordinator = %coordinator,
            devices = devices.len(),
            ?outcome,
            "runtime starting"
        );

        let (events_out, _) = broadcast::channel::<Event>(self.event_capacity);
        let (requests_tx, requests_rx) = mpsc::channel(64);

        let handle = Zigbee {
            requests: requests_tx,
            events: Arc::new(events_out.subscribe()),
            coordinator,
            start_outcome: outcome,
        };

        task::spawn(
            task::Config {
                adapter: self.adapter,
                store: self.store,
                adapter_events: self.adapter_events,
                requests: requests_rx,
                events: events_out,
                reachability: self.reachability,
                interview_on_join: self.interview_on_join,
                devices,
                outcome,
                coordinator,
                network_known: stored.is_some(),
                registry: self.registry,
                definitions: self.definitions,
            },
            handle.clone(),
        );

        Ok(handle)
    }
}

/// A running rszigbee runtime.
///
/// Cheap to clone; every clone talks to the same task.
#[derive(Debug, Clone)]
pub struct Zigbee {
    requests: mpsc::Sender<Request>,
    /// A receiver, not a sender, and this matters: a broadcast channel closes
    /// when its last **sender** drops. The task holds the only sender, so when
    /// the runtime stops every [`EventStream`] ends instead of hanging. A
    /// handle that held a sender would keep the channel open forever, and a
    /// `while let Some(event) = stream.recv().await` loop would never
    /// terminate after `stop()`.
    events: Arc<broadcast::Receiver<Event>>,
    coordinator: Ieee,
    start_outcome: StartOutcome,
}

impl Zigbee {
    /// Starts building a runtime over `adapter`, its event receiver, and
    /// `store`.
    pub fn builder<A: CoordinatorAdapter, S: ZigbeeStore>(
        adapter: A,
        adapter_events: mpsc::Receiver<AdapterEvent>,
        store: S,
    ) -> ZigbeeBuilder<A, S> {
        ZigbeeBuilder {
            adapter,
            adapter_events,
            store,
            network: NetworkConfig {
                pan_id: None,
                extended_pan_id: None,
                channel: DEFAULT_CHANNEL,
                network_key: None,
                // Refusing to form is the only safe default: forming when we
                // should have resumed orphans every joined device, and that is
                // not recoverable without re-pairing all of them.
                on_mismatch: MismatchPolicy::Fail,
            },
            backup: None,
            reachability: Arc::new(SilencePolicy::default()),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            interview_on_join: true,
            registry: rszigbee_spec::zcl::registry::ClusterRegistry::with_builtins(),
            definitions: rszigbee_devices::DefinitionIndex::new(),
        }
    }

    /// The coordinator's permanent address.
    #[must_use]
    pub fn coordinator(&self) -> Ieee {
        self.coordinator
    }

    /// What starting the network did.
    ///
    /// [`StartOutcome::Formed`] means every device previously joined to this
    /// coordinator is now orphaned, so it is worth surfacing to an operator
    /// rather than logging.
    ///
    /// Available here as well as through [`Event::Started`] because the task
    /// emits that event only once it is running, which is after `start()`
    /// returns — a stream subscribed immediately afterwards can miss it.
    #[must_use]
    pub fn start_outcome(&self) -> StartOutcome {
        self.start_outcome
    }

    /// Subscribes to events. Each call returns an independent stream that sees
    /// every event from this point on.
    ///
    /// The stream ends when the runtime stops, so a `while let Some(event)`
    /// loop terminates rather than hanging.
    #[must_use]
    pub fn events(&self) -> EventStream {
        EventStream {
            inner: self.events.resubscribe(),
        }
    }

    /// Every known device, in address order.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Devices(tx)).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)
    }

    /// One device, or `None` if it is not known.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped.
    pub async fn device(&self, ieee: Ieee) -> Result<Option<DeviceInfo>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Device(ieee, tx)).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)
    }

    /// The coordinator's current network parameters.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped or the coordinator cannot answer.
    pub async fn network(&self) -> Result<crate::adapter::NetworkInfo, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Network(tx)).await?;
        rx.await
            .map_err(|_| RuntimeError::Stopped)?
            .map_err(Into::into)
    }

    /// Opens joining for `duration`, optionally through one router.
    ///
    /// Passing [`Duration::ZERO`] closes it.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped, if `via` names a device the runtime
    /// does not know, or if the coordinator refuses.
    pub async fn permit_join(
        &self,
        duration: Duration,
        via: Option<Ieee>,
    ) -> Result<(), RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::PermitJoin {
            duration,
            via,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| RuntimeError::Stopped)?
    }

    /// Reads attributes from a device and waits for the response.
    ///
    /// A foundation read, so it works on a device with no definition. That is
    /// the point: reading `genBasic` is how a model string is learned, and the
    /// model is what resolves a definition.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped, the device is unknown, the coordinator
    /// refuses, or no response arrives in time.
    pub async fn zcl_read(
        &self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: Vec<AttrId>,
    ) -> Result<Vec<(u16, ZclValue)>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::ZclRead {
            ieee,
            endpoint,
            cluster,
            attributes,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| RuntimeError::Stopped)?
    }

    /// Sends a ZDO request and waits for the matching response.
    ///
    /// `build` receives the transaction sequence number, which the runtime
    /// allocates. The response is matched on that number, so the sequence
    /// cannot be chosen by a caller who then fails to encode it.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped, the device is unknown, the coordinator
    /// refuses the request, or no response arrives in time.
    pub async fn zdo(
        &self,
        ieee: Ieee,
        cluster: ZdoClusterId,
        build: impl FnOnce(u8) -> Vec<u8> + Send + 'static,
    ) -> Result<Vec<u8>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Zdo {
            ieee,
            cluster,
            build: Box::new(build),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| RuntimeError::Stopped)?
    }

    /// Sends a command to a device.
    ///
    /// # Errors
    ///
    /// Capability-level commands return [`CommandError::NoDefinition`] until
    /// the device-definition engine exists; see the module documentation. The
    /// ZCL escape hatches work now.
    pub async fn send(
        &self,
        ieee: Ieee,
        command: DeviceCommand,
    ) -> Result<CommandOutcome, CommandError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Command {
            ieee,
            command: Box::new(command),
            reply: tx,
        })
        .await
        .map_err(|_| CommandError::ShuttingDown)?;
        rx.await.map_err(|_| CommandError::ShuttingDown)?
    }

    /// The definition resolved for a device, if one matched.
    ///
    /// Returns the model name and whether that definition is complete. An
    /// incomplete one still works for the capabilities it does describe; the
    /// flag is how a caller finds out that something about the device is not
    /// expressed rather than discovering it from behaviour.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped.
    pub async fn definition(&self, ieee: Ieee) -> Result<Option<(String, bool)>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Definition(ieee, tx)).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)
    }

    /// The bindings and reporting a device's definition asks for.
    ///
    /// Materialised rather than executed, so an operator can see what joining
    /// a device will do to it, and so it is testable without a radio.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped. A device with no definition yields an
    /// empty plan rather than an error: nothing is known to configure.
    pub async fn configure_plan(&self, ieee: Ieee) -> Result<Vec<ConfigureStep>, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::ConfigurePlan(ieee, tx)).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)
    }

    /// Interviews a device now, whether or not it has been interviewed before.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped or the device is unknown. A device that
    /// answers nothing produces [`InterviewOutcome`] with what was learned, not
    /// an error: a partial interview is often still usable, which is why
    /// upstream's quirk tables exist.
    pub async fn interview(&self, ieee: Ieee) -> Result<InterviewOutcome, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Interview { ieee, reply: tx }).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)?
    }

    /// Stops the coordinator and the runtime task, flushing the store.
    ///
    /// # Errors
    ///
    /// Fails if the coordinator or the store errors while shutting down. The
    /// task stops regardless.
    pub async fn stop(&self) -> Result<(), RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(Request::Stop(tx)).await?;
        rx.await.map_err(|_| RuntimeError::Stopped)?
    }

    /// Reports interview progress from the interview task.
    pub(crate) async fn interview_update(
        &self,
        ieee: Ieee,
        update: InterviewUpdate,
    ) -> Result<(), RuntimeError> {
        self.request(Request::InterviewUpdate {
            ieee,
            update: Box::new(update),
        })
        .await
    }

    async fn request(&self, request: Request) -> Result<(), RuntimeError> {
        self.requests
            .send(request)
            .await
            .map_err(|_| RuntimeError::Stopped)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rszigbee_spec::ids::{ClusterId, CommandId, EndpointId, Ieee, Nwk};
    use rszigbee_spec::zcl::ZclValue;

    use super::*;
    use crate::adapter::{AdapterEvent, MockAdapter, MockHandle, ZclRx};
    use crate::command::{DeviceCommand, ZclAttributeWrite, ZclCommand};
    use crate::device::InterviewState;
    use crate::event::{Event, ZclMessageKind};
    use crate::store::{MemoryStore, PersistedNetwork, ZigbeeStore};

    const DEVICE: Ieee = Ieee::new(0x0012_4b00_2218_9abc);
    const COORDINATOR: Ieee = Ieee::new(0x0012_4b00_2218_9abc);

    /// A runtime over a mock adapter and an in-memory store.
    async fn runtime() -> (Zigbee, MockHandle) {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .start()
            .await
            .expect("start");
        (zigbee, control)
    }

    /// Waits for the first event satisfying `want`, or panics after a second.
    async fn wait_for<T>(stream: &mut EventStream, want: impl Fn(&Event) -> Option<T>) -> T {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let Some(event) = stream.recv().await else {
                    panic!("the event stream closed before the expected event");
                };
                if let Some(found) = want(&event) {
                    return found;
                }
            }
        })
        .await
        .expect("timed out waiting for an event")
    }

    #[tokio::test]
    async fn starting_reports_what_it_did_and_who_the_coordinator_is() {
        let (zigbee, _control) = runtime().await;
        assert_eq!(zigbee.coordinator(), COORDINATOR);
        // Resumed, not formed. Forming when we should have resumed is the
        // outcome that orphans every device.
        assert_eq!(zigbee.start_outcome(), StartOutcome::Resumed);
        assert!(zigbee.devices().await.expect("devices").is_empty());
    }

    #[tokio::test]
    async fn the_start_outcome_is_available_without_racing_the_event() {
        // `Event::Started` is emitted by the task, which begins running only
        // after `start()` returns, so a stream subscribed afterwards may or may
        // not see it. That race is why the outcome is also a method: the caller
        // who must not miss it does not have to subscribe at all.
        let (zigbee, _control) = runtime().await;
        assert_eq!(zigbee.start_outcome(), StartOutcome::Resumed);
        assert_eq!(zigbee.start_outcome(), zigbee.clone().start_outcome());
    }

    #[tokio::test]
    async fn a_join_creates_a_device_and_persists_it() {
        let (adapter, control, events) = MockAdapter::new();
        let store = MemoryStore::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();
        drop(store);

        assert!(control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        }));

        let joined = wait_for(&mut stream, |e| match e {
            Event::DeviceJoined { ieee, nwk } => Some((*ieee, *nwk)),
            _ => None,
        })
        .await;
        assert_eq!(joined, (DEVICE, Nwk::new(0x1234)));

        let devices = zigbee.devices().await.expect("devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].ieee, DEVICE);
        // Unknown, not guessed. Guessing "router" would make a battery sensor
        // get probed forever.
        assert_eq!(devices[0].kind, crate::device::DeviceKind::Unknown);
    }

    #[tokio::test]
    async fn a_rejoin_at_a_new_short_address_is_reported_and_the_index_follows() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1111),
        });
        wait_for(&mut stream, |e| {
            matches!(e, Event::DeviceJoined { .. }).then_some(())
        })
        .await;

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x2222),
        });
        let (from, to) = wait_for(&mut stream, |e| match e {
            Event::DeviceAddressChanged { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .await;
        assert_eq!((from, to), (Nwk::new(0x1111), Nwk::new(0x2222)));

        // Still one device, not two. A short address is not an identity.
        let devices = zigbee.devices().await.expect("devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].nwk, Nwk::new(0x2222));
    }

    #[tokio::test]
    async fn a_frame_from_a_known_device_decodes_into_a_typed_event() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        wait_for(&mut stream, |e| {
            matches!(e, Event::DeviceJoined { .. }).then_some(())
        })
        .await;

        // A genOnOff attribute report: onOff (0x0000) is a boolean, true.
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0006),
            group: None,
            was_broadcast: false,
            link_quality: Some(180),
            frame: vec![0x18, 0x01, 0x0a, 0x00, 0x00, 0x10, 0x01],
        }));

        let attributes = wait_for(&mut stream, |e| match e {
            Event::ZclMessage(m) => match &m.kind {
                ZclMessageKind::Attributes(a) => Some(a.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;
        assert_eq!(attributes, vec![(0x0000, ZclValue::Bool(true))]);
    }

    #[tokio::test]
    async fn a_frame_that_will_not_decode_still_produces_an_event_with_the_bytes() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        wait_for(&mut stream, |e| {
            matches!(e, Event::DeviceJoined { .. }).then_some(())
        })
        .await;

        // A report claiming an attribute follows, then nothing.
        let truncated = vec![0x18, 0x01, 0x0a, 0x00];
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0006),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame: truncated.clone(),
        }));

        // The bytes survive, because they are the only evidence anyone has for
        // adding support for whatever sent them.
        let raw = wait_for(&mut stream, |e| match e {
            Event::UnparsedFrame { raw, .. } => Some(raw.clone()),
            _ => None,
        })
        .await;
        assert_eq!(raw, truncated);
    }

    #[tokio::test]
    async fn a_capability_command_is_refused_rather_than_guessed() {
        let (zigbee, control) = runtime().await;
        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        // Wait for the device to exist before commanding it.
        let mut stream = zigbee.events();
        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        wait_for(&mut stream, |e| {
            matches!(
                e,
                Event::DeviceJoined { .. } | Event::DeviceAnnounced { .. }
            )
            .then_some(())
        })
        .await;

        // No definition engine yet, so this must be an error naming that, not
        // a frame sent to a guessed cluster.
        let err = zigbee
            .send(DEVICE, DeviceCommand::SetOn(true))
            .await
            .expect_err("a capability write cannot work without a definition");
        assert!(matches!(err, CommandError::NoDefinition), "{err:?}");
    }

    #[tokio::test]
    async fn a_zcl_escape_hatch_command_is_encoded_from_the_registry() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();
        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        wait_for(&mut stream, |e| {
            matches!(e, Event::DeviceJoined { .. }).then_some(())
        })
        .await;

        control.reply_zcl(Ok(None));
        zigbee
            .send(
                DEVICE,
                DeviceCommand::Zcl(ZclCommand {
                    endpoint: Some(EndpointId(1)),
                    cluster: ClusterId(0x0006),
                    command: CommandId(0x01), // on
                    params: Vec::new(),
                    manufacturer: None,
                    disable_default_response: false,
                }),
            )
            .await
            .expect("the escape hatch works without a definition");

        let sent = control.zcl_sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].cluster, ClusterId(0x0006));
        // Cluster-specific, client to server, command 0x01. The last byte is
        // what actually turns the light on.
        assert_eq!(sent[0].frame.first(), Some(&0x01));
        assert_eq!(sent[0].frame.last(), Some(&0x01));
    }

    #[tokio::test]
    async fn an_attribute_write_takes_its_wire_type_from_the_registry() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();
        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(DEVICE),
            nwk: Nwk::new(0x1234),
        });
        wait_for(&mut stream, |e| {
            matches!(e, Event::DeviceJoined { .. }).then_some(())
        })
        .await;

        control.reply_zcl(Ok(None));
        let result = zigbee
            .send(
                DEVICE,
                DeviceCommand::ZclAttributes(ZclAttributeWrite {
                    endpoint: Some(EndpointId(1)),
                    cluster: ClusterId(0x0000),
                    attributes: vec![(AttrId(0x0010), ZclValue::Str("hall".into()))],
                    manufacturer: None,
                }),
            )
            .await;

        // Whether 0x0010 is in the built-in registry decides which of these is
        // right, and either is a correct outcome -- what must not happen is a
        // frame with a type tag that does not match the value.
        match result {
            Ok(_) | Err(CommandError::InvalidValue { .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_store_from_a_different_coordinator_is_refused() {
        let store = MemoryStore::new();
        store
            .save_network(&PersistedNetwork {
                pan_id: 0x1a62,
                extended_pan_id: 0x94a0_81ff_fed9_6e5c,
                channel: 11,
                nwk_update_id: 0,
                // Not the mock's address.
                coordinator_ieee: Ieee::new(0xdead_beef_dead_beef),
                key_sequence: 0,
                frame_counter: 0,
            })
            .await
            .expect("save");

        let (adapter, _control, events) = MockAdapter::new();
        let error = Zigbee::builder(adapter, events, store)
            .start()
            .await
            .expect_err("a swapped coordinator must be refused, not reconciled");

        // The distinction that matters: every device's link key was derived
        // against the old coordinator, so continuing is unrecoverable.
        assert!(
            matches!(error, RuntimeError::CoordinatorMismatch { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn devices_are_restored_from_the_store_on_start() {
        let store = MemoryStore::new();
        let mut stored = crate::store::PersistedDevice::new(DEVICE, Nwk::new(0x4321));
        stored.interview = InterviewState::Successful;
        store.upsert_device(&stored).await.expect("upsert");

        let (adapter, _control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, store)
            .start()
            .await
            .expect("start");

        let devices = zigbee.devices().await.expect("devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].ieee, DEVICE);
        assert_eq!(devices[0].nwk, Nwk::new(0x4321));
        // Restored as already interviewed, so a restart does not re-interview
        // every device on the network.
        assert_eq!(devices[0].interview, InterviewState::Successful);
    }

    #[tokio::test]
    async fn permit_join_through_an_unknown_router_is_refused() {
        let (zigbee, control) = runtime().await;
        let error = zigbee
            .permit_join(Duration::from_secs(60), Some(Ieee::new(0x1)))
            .await
            .expect_err("an unknown router cannot be resolved to a short address");
        assert!(matches!(error, RuntimeError::UnknownDevice(_)), "{error:?}");
        assert!(control.permit_join_calls().is_empty());
    }

    #[tokio::test]
    async fn permit_join_reaches_the_adapter_and_is_reported() {
        let (zigbee, control) = runtime().await;
        let mut stream = zigbee.events();
        zigbee
            .permit_join(Duration::from_secs(60), None)
            .await
            .expect("permit join");

        let permitted = wait_for(&mut stream, |e| match e {
            Event::PermitJoinChanged { permitted, .. } => Some(*permitted),
            _ => None,
        })
        .await;
        assert!(permitted);
        assert_eq!(control.permit_join_calls().len(), 1);
    }

    #[tokio::test]
    async fn stopping_stops_the_adapter_and_closes_the_stream() {
        let (zigbee, control) = runtime().await;
        let mut stream = zigbee.events();
        zigbee.stop().await.expect("stop");

        let stopping = wait_for(&mut stream, |e| matches!(e, Event::Stopping).then_some(())).await;
        let () = stopping;
        assert!(!control.is_started());

        // The stream ends rather than hanging, so a consumer loop terminates.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.recv())
                .await
                .expect("the stream must close")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_request_after_stopping_reports_that_rather_than_hanging() {
        let (zigbee, _control) = runtime().await;
        zigbee.stop().await.expect("stop");
        // Give the task a moment to drop its receiver.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let error = zigbee
            .devices()
            .await
            .expect_err("a stopped runtime must refuse, not hang");
        assert!(matches!(error, RuntimeError::Stopped), "{error:?}");
    }

    #[tokio::test]
    async fn a_zdo_request_to_an_unknown_device_is_refused_before_the_radio() {
        let (zigbee, control) = runtime().await;
        let error = zigbee
            .zdo(Ieee::new(0x99), ZdoClusterId::NODE_DESC_REQ, |seq| {
                vec![seq]
            })
            .await
            .expect_err("there is no short address to send to");
        assert!(matches!(error, RuntimeError::UnknownDevice(_)), "{error:?}");
        assert!(control.zdo_sent().is_empty());
    }
}

/// The vertical slice: a definition producing behaviour.
///
/// These are the acceptance criteria for wiring definitions into the runtime.
/// Each one is a claim that could quietly stop being true, and all of them run
/// against `MockAdapter` with no hardware.
#[cfg(test)]
mod definition_integration {
    use std::time::Duration;

    use rszigbee_devices::{Definition, DefinitionIndex, Extend};
    use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, Nwk};

    use super::*;
    use crate::adapter::{AdapterEvent, MockAdapter, MockHandle, ZclRx};
    use crate::command::DeviceCommand;
    use crate::device::{DeviceKind, InterviewState};
    use crate::event::Event;
    use crate::store::{MemoryStore, PersistedDevice, PersistedEndpoint, ZigbeeStore};

    const BULB: Ieee = Ieee::new(0x0017_8801_00dc_4d3f);
    const SENSOR: Ieee = Ieee::new(0x0012_4b00_2218_9abc);

    /// A complete definition for a dimmable light.
    fn bulb_definition() -> Definition {
        let mut d = Definition::new("TRADFRI bulb E27 WS opal 980lm");
        d.vendor = "IKEA".into();
        d.match_rules.models = vec!["TRADFRI bulb E27 WS opal 980lm".into()];
        d.extend = vec![
            Extend::Light {
                brightness: true,
                color_temp: Some((250, 454)),
                color: false,
            },
            Extend::Identify,
        ];
        let mut binding = rszigbee_devices::Binding::default();
        binding.endpoint = EndpointId(1);
        binding.cluster = ClusterId(0x0006);
        binding.reporting = vec![rszigbee_devices::Reporting::default()];
        d.bindings = vec![binding];
        d
    }

    /// A definition for a sensor, which has no on/off.
    fn sensor_definition() -> Definition {
        let mut d = Definition::new("TS0601_soil");
        d.match_rules.models = vec!["TS0601".into()];
        d.extend = vec![Extend::Temperature(rszigbee_devices::NumericSpec::default())];
        d
    }

    fn index() -> DefinitionIndex {
        let mut index = DefinitionIndex::new();
        index.insert(bulb_definition()).expect("insert");
        index.insert(sensor_definition()).expect("insert");
        index
    }

    /// A store already holding an interviewed device, so the tests exercise
    /// resolution without re-running an interview the mock cannot script.
    async fn stored(ieee: Ieee, model: &str, clusters: &[u16]) -> MemoryStore {
        let store = MemoryStore::new();
        let mut device = PersistedDevice::new(ieee, Nwk::new(0x1234));
        device.kind = DeviceKind::Router;
        device.interview = InterviewState::Successful;
        device.basic.model_id = Some(model.to_owned());
        device.endpoints = vec![PersistedEndpoint {
            id: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            device_id: 0x0100,
            input_clusters: clusters.iter().copied().map(ClusterId).collect(),
            output_clusters: Vec::new(),
        }];
        store.upsert_device(&device).await.expect("upsert");
        store
    }

    async fn runtime_with(store: MemoryStore) -> (Zigbee, MockHandle) {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, store)
            .definitions(index())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        (zigbee, control)
    }

    // ---- 1. a known complete definition no longer returns NoDefinition

    #[tokio::test]
    async fn a_recognised_device_resolves_to_its_definition() {
        let (zigbee, _control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0006, 0x0008]).await)
                .await;
        let resolved = zigbee.definition(BULB).await.expect("definition");
        assert_eq!(
            resolved,
            Some(("TRADFRI bulb E27 WS opal 980lm".to_owned(), true)),
            "the model was learned, so a definition must match and be complete"
        );
    }

    // ---- 2. SetOn produces the expected genOnOff command

    #[tokio::test]
    async fn set_on_reaches_the_radio_as_a_gen_on_off_command() {
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0006, 0x0008]).await)
                .await;
        control.reply_zcl(Ok(None));

        zigbee
            .send(BULB, DeviceCommand::SetOn(true))
            .await
            .expect("a recognised light accepts on/off");

        let sent = control.zcl_sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].cluster, ClusterId(0x0006));
        assert_eq!(sent[0].endpoint, EndpointId(1));
        // Frame control 0x01 is cluster-specific client-to-server; the last
        // byte is the command, 0x01 = on.
        assert_eq!(sent[0].frame.first(), Some(&0x01));
        assert_eq!(sent[0].frame.last(), Some(&0x01));
    }

    #[tokio::test]
    async fn set_off_differs_from_set_on_only_in_the_command_byte() {
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0006]).await).await;
        control.reply_zcl(Ok(None));
        zigbee
            .send(BULB, DeviceCommand::SetOn(false))
            .await
            .expect("off");
        let sent = control.zcl_sent();
        assert_eq!(sent[0].frame.last(), Some(&0x00));
    }

    // ---- 3. endpoint mapping is respected

    #[tokio::test]
    async fn the_definitions_declared_endpoint_is_used_not_the_first_cluster_host() {
        // A two-gang switch where both endpoints host genOnOff. Only the
        // definition knows gang two is the one being addressed, and picking the
        // first host would switch the wrong one.
        let mut definition = Definition::new("two gang");
        definition.match_rules.models = vec!["TS0002".into()];
        definition.extend = vec![Extend::OnOff {
            endpoints: vec![EndpointId(2)],
            power_on_behavior: false,
        }];
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let store = MemoryStore::new();
        let mut device = PersistedDevice::new(SENSOR, Nwk::new(0x1234));
        device.interview = InterviewState::Successful;
        device.basic.model_id = Some("TS0002".into());
        device.endpoints = (1u8..=2)
            .map(|id| PersistedEndpoint {
                id: EndpointId(id),
                profile: rszigbee_spec::ids::ProfileId::HA,
                device_id: 0x0100,
                input_clusters: vec![ClusterId(0x0006)],
                output_clusters: Vec::new(),
            })
            .collect();
        store.upsert_device(&device).await.expect("upsert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, store)
            .definitions(index)
            .interview_on_join(false)
            .start()
            .await
            .expect("start");

        control.reply_zcl(Ok(None));
        zigbee
            .send(SENSOR, DeviceCommand::SetOn(true))
            .await
            .expect("send");
        assert_eq!(control.zcl_sent()[0].endpoint, EndpointId(2));
    }

    // ---- 4. the configure plan can be materialised

    #[tokio::test]
    async fn the_configure_plan_is_materialised_from_the_definition() {
        let (zigbee, _control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0006]).await).await;
        let plan = zigbee.configure_plan(BULB).await.expect("plan");
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert_eq!(plan[0].cluster, ClusterId(0x0006));
        assert_eq!(plan[0].endpoint, EndpointId(1));
        assert!(
            plan[0].max_interval > 0,
            "without a max interval a silent device is indistinguishable from a dead one"
        );
    }

    // ---- 5. unknown or unsupported never silently falls back

    #[tokio::test]
    async fn an_unrecognised_device_refuses_the_command_explicitly() {
        let (zigbee, control) =
            runtime_with(stored(BULB, "NOT-IN-THE-CATALOGUE", &[0x0006]).await).await;

        assert_eq!(zigbee.definition(BULB).await.expect("definition"), None);
        let error = zigbee
            .send(BULB, DeviceCommand::SetOn(true))
            .await
            .expect_err("an unrecognised device must not be guessed at");
        assert!(matches!(error, CommandError::NoDefinition), "{error:?}");
        // The important half: nothing reached the radio.
        assert!(
            control.zcl_sent().is_empty(),
            "a refused command must not send anything"
        );
    }

    #[tokio::test]
    async fn a_sensor_refuses_on_off_rather_than_sending_gen_on_off_anyway() {
        let (zigbee, control) =
            runtime_with(stored(SENSOR, "TS0601", &[0x0000, 0x0402]).await).await;
        assert_eq!(
            zigbee.definition(SENSOR).await.expect("definition"),
            Some(("TS0601_soil".to_owned(), true))
        );

        let error = zigbee
            .send(SENSOR, DeviceCommand::SetOn(true))
            .await
            .expect_err("a soil sensor has no on/off");
        assert!(
            matches!(error, CommandError::UnsupportedCapability(ref c) if c.as_str() == "state"),
            "{error:?}"
        );
        assert!(control.zcl_sent().is_empty());
    }

    #[tokio::test]
    async fn a_device_with_no_model_learned_yet_resolves_to_nothing() {
        // Joined but not interviewed: no model string, so no definition. This
        // must be a clean "not yet" rather than a wrong match.
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .definitions(index())
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(BULB),
            nwk: Nwk::new(0x1234),
        });
        let _ = stream.recv().await;

        assert_eq!(zigbee.definition(BULB).await.expect("definition"), None);
    }

    #[tokio::test]
    async fn an_incomplete_definition_reports_itself_and_still_serves_what_it_describes() {
        // A deliberate reading of "incomplete must fail explicitly": the
        // failure is per *capability*, not per definition.
        //
        // Refusing every command on a definition carrying one
        // `Extend::Unsupported` would break a light because its vendor effects
        // are not expressed, which helps nobody. What must never happen is a
        // silent fallback — and that is enforced by the capability mapping
        // itself, which only ever emits what the definition states.
        //
        // So incompleteness is *reported*, through `definition()` and a log
        // line, and the capabilities the definition does describe keep working.
        let mut definition = bulb_definition();
        definition.extend.push(Extend::Unsupported {
            helper: "philips.m.gradient".into(),
            note: "gradient effects need a converter".into(),
        });
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(
            adapter,
            events,
            stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0006]).await,
        )
        .definitions(index)
        .interview_on_join(false)
        .start()
        .await
        .expect("start");

        // Visible as incomplete...
        assert_eq!(
            zigbee.definition(BULB).await.expect("definition"),
            Some(("TRADFRI bulb E27 WS opal 980lm".to_owned(), false)),
            "the second element is `is_complete`, and it must be false"
        );

        // ...and still a working light.
        control.reply_zcl(Ok(None));
        zigbee
            .send(BULB, DeviceCommand::SetOn(true))
            .await
            .expect("on/off is described, so it must work");
        assert_eq!(control.zcl_sent().len(), 1);

        // But the part that is not expressed is refused, not approximated.
        let error = zigbee
            .send(BULB, DeviceCommand::SetPreset("gradient".into()))
            .await
            .expect_err("an unexpressed capability must be refused");
        assert!(matches!(error, CommandError::NoDefinition), "{error:?}");
    }

    // ---- the read path, which is what makes a model string exist at all

    #[tokio::test]
    async fn a_gen_basic_read_is_correlated_by_transaction_sequence() {
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0000]).await).await;

        // The adapter accepts the read and answers out of band, as the Ember
        // one does.
        control.reply_zcl(Ok(None));
        let handle = tokio::spawn({
            let zigbee = zigbee.clone();
            async move {
                zigbee
                    .zcl_read(BULB, EndpointId(1), ClusterId(0x0000), vec![AttrId(0x0005)])
                    .await
            }
        });

        // Wait for the request to actually be sent, then answer it with the
        // sequence number the runtime chose.
        let tsn = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(tx) = control.zcl_sent().first()
                    && let Some(&tsn) = tx.frame.get(1)
                {
                    return tsn;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the read should reach the adapter");

        // A read response: frame control 0x18 (server to client), the same tsn,
        // command 0x01, then attribute 0x0005 status 0 type 0x42 "bulb".
        let mut frame = vec![0x18, tsn, 0x01, 0x05, 0x00, 0x00, 0x42, 0x04];
        frame.extend_from_slice(b"bulb");
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(BULB),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0000),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));

        let values = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("no timeout")
            .expect("task")
            .expect("the read must be answered by the correlated frame");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, 0x0005);
    }

    #[tokio::test]
    async fn a_correlated_read_response_is_not_also_reported_as_an_attribute_report() {
        // Otherwise every read would look like an unsolicited report and a
        // consumer would see phantom state changes.
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0000]).await).await;
        let mut stream = zigbee.events();
        control.reply_zcl(Ok(None));

        let reader = tokio::spawn({
            let zigbee = zigbee.clone();
            async move {
                zigbee
                    .zcl_read(BULB, EndpointId(1), ClusterId(0x0000), vec![AttrId(0x0005)])
                    .await
            }
        });
        let tsn = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(&tsn) = control.zcl_sent().first().and_then(|t| t.frame.get(1)) {
                    return tsn;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("sent");

        let mut frame = vec![0x18, tsn, 0x01, 0x05, 0x00, 0x00, 0x42, 0x04];
        frame.extend_from_slice(b"bulb");
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(BULB),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0000),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));
        let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;

        // Drain briefly: there must be no ZclMessage for the correlated frame.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(30), stream.recv()).await {
                Ok(Some(Event::ZclMessage(m))) => {
                    panic!("a correlated read answer must not surface as a report: {m:?}")
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}
