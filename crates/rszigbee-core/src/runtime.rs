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
//! # What a command needs before it can work
//!
//! Capability-level commands — `SetOn`, `SetBrightness`, and the rest of
//! [`DeviceCommand`] — are lowered through the device's resolved definition,
//! because only that says which cluster and attribute a capability lives on. A
//! device with no definition, or one whose definition does not give it that
//! capability, is refused with [`CommandError::NoDefinition`] or
//! [`CommandError::UnsupportedCapability`] and nothing reaches the radio.
//!
//! There is deliberately no fallback. A guess that is right on most devices is
//! silently wrong on the rest, and those are the failures nobody can diagnose.
//!
//! [`DeviceCommand::Zcl`] and [`DeviceCommand::ZclAttributes`] need no
//! definition: they are already expressed in the terms the adapter takes, and
//! the runtime encodes them from the cluster registry.

mod behavior;
mod behaviors;
mod decode;
mod definitions;
mod encode;
mod interview;
mod inventory;
mod task;
mod tuya;

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

pub use behavior::{
    BehaviorRegistry, ConfigureContext, DecodeContext, DeviceBehavior, EncodeContext, Outcome,
};
pub use behaviors::TuyaThermostatSchedule;
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

    /// A device refused an attribute read.
    ///
    /// Distinct from a timeout: the device answered, and said no. A caller can
    /// act on that — the attribute is unsupported, or needs a manufacturer
    /// code — where a timeout only says to try again.
    #[error("{ieee} refused the read with ZCL status 0x{status:02x}")]
    ReadRefused {
        /// Which device.
        ieee: Ieee,
        /// The ZCL status byte it answered with.
        status: u8,
    },

    /// A device did not answer a ZDO request in time.
    #[error("no ZDO response from {ieee} within {timeout:?}")]
    ZdoTimeout {
        /// Which device.
        ieee: Ieee,
        /// How long was allowed.
        timeout: Duration,
    },

    /// A device did not answer an attribute read in time.
    ///
    /// Separate from [`RuntimeError::ZdoTimeout`] because the two say different
    /// things about a device: ZDO is answered by the stack, so silence there
    /// suggests the device is gone, while a ZCL read is answered by the
    /// application on it. Reporting a ZCL timeout as a ZDO one sent a reader
    /// looking in the wrong layer.
    #[error("no ZCL response from {ieee} within {timeout:?}")]
    ZclTimeout {
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
    Configure(
        Ieee,
        oneshot::Sender<Result<ConfigureOutcome, RuntimeError>>,
    ),
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

/// What executing a configure plan achieved.
///
/// Both numbers are reported because a partial result is the common one: a
/// device that refuses one binding and accepts three is configured well enough
/// to work, and a caller that only saw "failed" would retry pointlessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConfigureOutcome {
    /// Bindings the device accepted.
    pub bound: usize,
    /// Attributes whose reporting was configured.
    pub configured: usize,
    /// Steps that failed.
    pub failed: usize,
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
    behaviors: BehaviorRegistry,
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

    /// Adds a named behaviour a definition can delegate to.
    ///
    /// The shipped behaviours are present by default. This is for behaviour a
    /// caller implements themselves — a device nobody has contributed a
    /// definition for, or one whose quirk is specific to a deployment.
    ///
    /// A behaviour is attached to part of a definition, not to a whole device:
    /// everything the declarative table can express keeps going through it, so
    /// the device stays maintained by the importer.
    #[must_use]
    pub fn behavior(mut self, behavior: impl DeviceBehavior) -> Self {
        self.behaviors.insert(behavior);
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
                behaviors: self.behaviors,
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
            behaviors: BehaviorRegistry::with_builtins(),
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
        self.ask(Request::Devices).await
    }

    /// One device, or `None` if it is not known.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped.
    pub async fn device(&self, ieee: Ieee) -> Result<Option<DeviceInfo>, RuntimeError> {
        self.ask(|reply| Request::Device(ieee, reply)).await
    }

    /// The coordinator's current network parameters.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped or the coordinator cannot answer.
    pub async fn network(&self) -> Result<crate::adapter::NetworkInfo, RuntimeError> {
        self.ask(Request::Network).await?.map_err(Into::into)
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
        self.ask(|reply| Request::PermitJoin {
            duration,
            via,
            reply,
        })
        .await?
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
        self.ask(|reply| Request::ZclRead {
            ieee,
            endpoint,
            cluster,
            attributes,
            reply,
        })
        .await?
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
        self.ask(|reply| Request::Zdo {
            ieee,
            cluster,
            build: Box::new(build),
            reply,
        })
        .await?
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
        self.ask(|reply| Request::Command {
            ieee,
            command: Box::new(command),
            reply,
        })
        .await
        .map_err(|_| CommandError::ShuttingDown)?
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
        self.ask(|reply| Request::Definition(ieee, reply)).await
    }

    /// Executes the device's configure plan: bind, then configure reporting.
    ///
    /// Run automatically when an interview resolves a definition. Exposed as
    /// well because configuration does not always survive: a device that was
    /// power-cycled or rejoined may have lost its bindings, and re-running
    /// this is the fix. Idempotent — binding an already-bound cluster and
    /// reconfiguring reporting are both no-ops on the device.
    ///
    /// # Errors
    ///
    /// Fails if the runtime has stopped or the device is unknown. Individual
    /// step failures are counted in the outcome rather than failing the call:
    /// devices refuse bindings routinely, and abandoning the plan on the first
    /// refusal would leave later, working steps unconfigured.
    pub async fn configure(&self, ieee: Ieee) -> Result<ConfigureOutcome, RuntimeError> {
        self.ask(|reply| Request::Configure(ieee, reply)).await?
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
        self.ask(|reply| Request::ConfigurePlan(ieee, reply)).await
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
        self.ask(|reply| Request::Interview { ieee, reply }).await?
    }

    /// Stops the coordinator and the runtime task, flushing the store.
    ///
    /// # Errors
    ///
    /// Fails if the coordinator or the store errors while shutting down. The
    /// task stops regardless.
    pub async fn stop(&self) -> Result<(), RuntimeError> {
        self.ask(Request::Stop).await?
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

    /// Sends a request and waits for its answer.
    ///
    /// Every method on this handle is the same three steps — make a reply
    /// channel, send a request carrying its sender, await the answer — and
    /// spelling them out twelve times invited one of them to get the error
    /// mapping subtly wrong. `make` receives the sender so each caller only
    /// names its own variant.
    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Request,
    ) -> Result<T, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.request(make(tx)).await?;
        // A dropped sender means the task stopped between the send and the
        // reply, which is `Stopped` rather than a lost message.
        rx.await.map_err(|_| RuntimeError::Stopped)
    }

    /// Sends a request without waiting for an answer.
    async fn request(&self, request: Request) -> Result<(), RuntimeError> {
        self.requests
            .send(request)
            .await
            .map_err(|_| RuntimeError::Stopped)
    }
}
