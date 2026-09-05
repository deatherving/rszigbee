//! Silicon Labs `EmberZNet` coordinator adapter: EZSP over `ASHv2`.
//!
//! Verified against a Sonoff `ZBDongle-E` (`EFR32MG21`) running `EmberZNet` 7.4.4.0,
//! EZSP v13.
//!
//! # Why this adapter first
//!
//! Largest share of new `Zigbee2MQTT` installations, existing MIT Rust crates for
//! the transport, and — decisively — **no firmware reflash**, so an existing
//! network survives a migration. Contrast a host-side stack over an RCP radio,
//! which requires reflashing the dongle and orphaning every paired device.
//!
//! # ZCL responses arrive as events, not return values
//!
//! `send_zcl` returns `Ok(None)` after the APS layer confirms delivery. It does
//! **not** return the device's ZCL reply.
//!
//! This falsified an assumption in the design: the research said response
//! correlation "belongs to the adapter implementation, since it already knows
//! the transport's sequence numbers". For EZSP that is only half true. EZSP
//! correlates at the APS layer by message tag, which answers "did it arrive",
//! and it reports the reply separately through `incomingMessageHandler`. Nothing
//! at the transport layer links a request to a ZCL reply — that is the ZCL
//! transaction sequence number, which lives one layer up and is allocated by
//! the runtime.
//!
//! So the runtime correlates ZCL replies from [`AdapterEvent::Zcl`] using the
//! TSN it allocated. The trait signature keeps `Option<ZclRx>` because a
//! different transport may genuinely be able to correlate; Ember cannot, and
//! pretending otherwise would mean this adapter guessing which inbound frame
//! answered which request.

#![forbid(unsafe_code)]

pub mod bringup;
pub mod connection;
pub mod fingerprint;
pub mod form;
mod session;

use std::time::Duration;

use rsezsp::Eui64;
use rsezsp::ezsp::callback::Callback;
use rsezsp::ezsp::command::{
    ExportKey, GetEui64, GetNetworkKeyInfo, GetNetworkParameters, GetValue, NetworkState,
    PermitJoining, SendBroadcast, SendMulticast, SendUnicast,
};
use rsezsp::types::aps::{ApsFrame, ApsOptions, UnicastType};
use rsezsp::types::network::{NetworkStatus, NodeId, ValueId};
use rsezsp::types::security::SecurityManContext;

use crate::connection::{Connection, check, context};
use rszigbee_adapter::{
    AdapterCapabilities, AdapterError, AdapterEvent, CoordinatorAdapter, Destination, FirmwareInfo,
    MismatchPolicy, NetworkConfig, NetworkInfo, SecretKey, StartOutcome, ZclRx, ZclTx, ZdoTx,
};
use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, ManufacturerCode, Nwk};
use tracing::{debug, info, warn};

pub use fingerprint::{SerialSettings, recognise, settings_for};

/// The largest APS payload EZSP will accept in one frame. Beyond this the
/// stack needs APS fragmentation, which is not implemented yet.
const MAX_APS_PAYLOAD: usize = 255;

/// Hop limit for a multicast. Zero means the stack's default, which is the
/// maximum for the network's depth; a hand-picked number would silently
/// exclude devices further away than someone guessed.
const GROUP_HOPS: u8 = 0;

/// How far a multicast travels through nodes that are *not* group members.
///
/// A group's members are not necessarily neighbours, so a multicast has to be
/// relayed by nodes with no interest in it. Zero would confine the message to
/// members that happen to be in radio range.
const NONMEMBER_RADIUS: u8 = 7;

/// Hop limit for a broadcast. Zero is the stack default, again the maximum.
const BROADCAST_RADIUS: u8 = 0;

/// Silicon Labs' manufacturer code, used when the coordinator originates frames.
const SILABS: ManufacturerCode = ManufacturerCode(0x1049);

/// EZSP `sendUnicast` accepts a tag the NCP echoes in `messageSentHandler`.
/// Wraps at 255, which is fine: it only needs to be unique among in-flight
/// requests, and the NCP holds far fewer than 255.
#[derive(Debug, Default)]
struct MessageTag(u8);

impl MessageTag {
    fn next(&mut self) -> u8 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }
}

/// The `EmberZNet` adapter.
pub struct EmberAdapter {
    path: String,
    settings: SerialSettings,
    session: Option<session::Session>,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
    tag: MessageTag,
    coordinator: Option<Ieee>,
    formed: Option<form::Formed>,
}

impl std::fmt::Debug for EmberAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmberAdapter")
            .field("path", &self.path)
            .field("settings", &self.settings)
            .field("connected", &self.session.is_some())
            // `events`, `tag` and `coordinator` are deliberately omitted:
            // channel senders and a rolling counter are noise in a log line.
            .finish_non_exhaustive()
    }
}

impl EmberAdapter {
    /// An adapter for the given serial path, with settings from the fingerprint
    /// table (see [`fingerprint`] for why guessing is not an option).
    #[must_use]
    pub fn serial(path: impl Into<String>) -> EmberAdapterBuilder {
        let path = path.into();
        let settings = settings_for(&path);
        if let Some(r) = recognise(&path) {
            debug!(dongle = r.name, ?settings, "recognised coordinator");
        } else {
            debug!(path = %path, ?settings, "unrecognised coordinator, using fallback settings");
        }
        EmberAdapterBuilder { path, settings }
    }

    /// What forming produced, if this start formed a network.
    ///
    /// **The caller must persist [`form::Formed::network_key`].** Losing it loses the
    /// network: every device joined to it would have to be re-paired. Returns
    /// `None` when the network was resumed rather than formed, which is the
    /// normal case.
    #[must_use]
    pub const fn formed_network(&self) -> Option<&form::Formed> {
        self.formed.as_ref()
    }

    fn connection(&self) -> Result<&Connection, AdapterError> {
        self.session
            .as_ref()
            .map(|s| &s.connection)
            .ok_or(AdapterError::NotConnected)
    }

    /// Builds the APS frame for a ZCL request.
    ///
    /// `RETRY` and `ENABLE_ROUTE_DISCOVERY` are the options every working stack
    /// sets for a *unicast*: without retry a single lost frame looks like an
    /// unreachable device, and without route discovery the first message to a
    /// device behind a router fails.
    ///
    /// Neither applies to a group or a broadcast, and both are actively wrong
    /// there. APS retry waits for an acknowledgement, and a multicast or
    /// broadcast is never acknowledged -- there is no single recipient to
    /// acknowledge it -- so asking for one makes every send wait out a timeout
    /// and report a failure that did not happen. Route discovery has no route
    /// to discover.
    fn aps_frame_for(req: &ZclTx, sequence: u8) -> ApsFrame {
        let mut options = ApsOptions(0);
        let group = match req.dest {
            Destination::Unicast { .. } => {
                options = options.union(ApsOptions::RETRY);
                if !req.options.disable_recovery {
                    options = options.union(ApsOptions::ENABLE_ROUTE_DISCOVERY);
                }
                0
            }
            // The group id travels in the APS frame, not in a destination
            // argument: `sendMulticast` takes no address and reads the group
            // from here.
            Destination::Group(group) => group.0,
            Destination::Broadcast(_) => 0,
        };
        ApsFrame {
            profile_id: req.profile.0,
            cluster_id: req.cluster.0,
            source_endpoint: req.source_endpoint.0,
            destination_endpoint: req.endpoint.0,
            options,
            group_id: group,
            sequence,
        }
    }

    /// Derives the start outcome from the coordinator's reported network state.
    ///
    /// The safety-critical decision in this adapter. `MismatchPolicy::Fail` is
    /// the default because forming a network when we should have resumed one
    /// orphans every device the user owns, and that is not recoverable without
    /// re-pairing all of them.
    fn outcome_for(
        state: NetworkStatus,
        policy: MismatchPolicy,
    ) -> Result<StartOutcome, AdapterError> {
        match state {
            s if s.is_joined() => Ok(StartOutcome::Resumed),
            NetworkStatus::NO_NETWORK => match policy {
                MismatchPolicy::Form => Ok(StartOutcome::Formed),
                MismatchPolicy::Fail => Err(AdapterError::NetworkMismatch(
                    "the coordinator has no network formed. Forming one would create a \
                     new network that no existing device is joined to; pass \
                     MismatchPolicy::Form to do that deliberately."
                        .into(),
                )),
            },
            // Mid-transition. Resuming from here would race the stack.
            other => Err(AdapterError::NetworkMismatch(format!(
                "coordinator is in a transient network state ({other}); retry shortly"
            ))),
        }
    }
}

/// Builder for [`EmberAdapter`].
#[derive(Debug, Clone)]
pub struct EmberAdapterBuilder {
    path: String,
    settings: SerialSettings,
}

impl EmberAdapterBuilder {
    /// Overrides the baud rate.
    #[must_use]
    pub const fn baud(mut self, baud: u32) -> Self {
        self.settings.baud = baud;
        self
    }

    /// Overrides hardware flow control.
    ///
    /// Enabling this on a dongle that does not wire RTS/CTS makes `open(2)`
    /// block in the kernel indefinitely. See [`fingerprint`].
    #[must_use]
    pub const fn rtscts(mut self, on: bool) -> Self {
        self.settings.rtscts = on;
        self
    }

    /// Builds the adapter and the receiver its events arrive on.
    #[must_use]
    pub fn build(self) -> (EmberAdapter, tokio::sync::mpsc::Receiver<AdapterEvent>) {
        // Bounded: a full channel must drop with a counter, never grow.
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        (
            EmberAdapter {
                path: self.path,
                settings: self.settings,
                session: None,
                events: tx,
                tag: MessageTag::default(),
                coordinator: None,
                formed: None,
            },
            rx,
        )
    }
}

impl CoordinatorAdapter for EmberAdapter {
    async fn start(
        &mut self,
        network: &NetworkConfig,
        _backup: Option<&[u8]>,
    ) -> Result<StartOutcome, AdapterError> {
        let mut session = session::connect(&self.path, self.settings).await?;

        let eui = session
            .connection
            .command(GetEui64)
            .await
            .map_err(|e| context("cannot read the coordinator address", &e))?
            .eui64;
        let coordinator = eui64_to_ieee(eui);

        // Order matters and is not arbitrary. EZSP rejects `addEndpoint` once
        // the stack is running, so endpoints and identity must be configured
        // before the network comes up.
        bringup::configure_endpoints(&session.connection, SILABS).await?;

        // Stack configuration, before the network comes up: EZSP refuses these
        // writes once it is running. Stack profile, security level and
        // end-device capacity are advertised in every beacon, and a device
        // whose scan reads the wrong value never attempts to associate -- so
        // getting these wrong produces silence, not an error.
        bringup::configure_stack(&session.connection).await?;

        // Trust-centre policies, also before the stack comes up. `permitJoining`
        // only opens the association window; whether a device is *admitted* is
        // a separate decision, and the EmberZNet default admits only devices
        // with a preconfigured key -- which no ordinary device has. Without
        // this, joining opened and nothing ever joined.
        bringup::configure_join_policies(&session.connection).await?;

        // Then resume any stored network. Skipping this makes `networkState`
        // report NoNetwork on a coordinator that has a perfectly good network,
        // which would lead a stack straight into forming over it. Observed on
        // real hardware.
        let stored = bringup::resume_stored_network(&session.connection).await?;

        let state = session
            .connection
            .command(NetworkState)
            .await
            .map_err(|e| context("cannot read the network state", &e))?
            .state;

        info!(
            ezsp = session.version,
            coordinator = %coordinator,
            ?stored,
            ?state,
            "Ember coordinator online"
        );

        let outcome = EmberAdapter::outcome_for(state, network.on_mismatch)?;
        if matches!(outcome, StartOutcome::Formed) {
            let formed = form::form(&session.connection, coordinator, network).await?;
            // The caller must persist `network_key`: losing it loses the
            // network, and every joined device would need re-pairing. Held here
            // so the runtime can read it back once persistence is wired.
            self.formed = Some(formed);
        }

        self.coordinator = Some(coordinator);

        // Callbacks are drained on their own task and translated to adapter
        // events. Taking the receiver out of the session means the session no
        // longer owns it, which is why it is moved rather than borrowed.
        let callbacks = std::mem::replace(&mut session.callbacks, tokio::sync::mpsc::channel(1).1);
        tokio::spawn(pump_callbacks(callbacks, self.events.clone()));

        self.session = Some(session);
        Ok(outcome)
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        // Dropping the session drops the ASH handle, which closes the outbound
        // queue and terminates the transmitter, then the receiver.
        self.session = None;
        self.coordinator = None;
        let _ = self
            .events
            .send(AdapterEvent::Disconnected(
                rszigbee_adapter::DisconnectReason::Requested,
            ))
            .await;
        Ok(())
    }

    async fn coordinator_ieee(&mut self) -> Result<Ieee, AdapterError> {
        if let Some(known) = self.coordinator {
            return Ok(known);
        }
        let eui = self
            .connection()?
            .command(GetEui64)
            .await
            .map_err(|e| context("cannot read the coordinator address", &e))?
            .eui64;
        let ieee = eui64_to_ieee(eui);
        self.coordinator = Some(ieee);
        Ok(ieee)
    }

    async fn firmware(&mut self) -> Result<FirmwareInfo, AdapterError> {
        let ezsp_version = self.session.as_ref().map_or(0, |s| s.version);
        let response = self
            .connection()?
            .command(GetValue {
                value_id: ValueId::VERSION_INFO,
            })
            .await
            .map_err(|e| context("cannot read the firmware version", &e))?;
        check("reading the firmware version", response.status)?;

        // VERSION_INFO is build (u16 LE), major, minor, patch, special, type.
        let version = match response.value.as_slice() {
            [b0, b1, major, minor, patch, special, ..] => {
                let build = u16::from(*b0) | (u16::from(*b1) << 8);
                format!("EmberZNet {major}.{minor}.{patch}.{special} build {build}")
            }
            other => format!("EmberZNet (unparsed version info {other:02x?})"),
        };

        Ok(FirmwareInfo {
            family: "ember".into(),
            version,
            meta: vec![("ezsp".into(), ezsp_version.to_string())],
        })
    }

    async fn network_info(&mut self) -> Result<NetworkInfo, AdapterError> {
        let network = self
            .connection()?
            .command(GetNetworkParameters)
            .await
            .map_err(|e| context("cannot read the network parameters", &e))?;
        check("reading the network parameters", network.status)?;
        let params = network.parameters;
        // The frame counter and key sequence are security-manager state, not
        // network parameters, so they need a second call. Worth making: the
        // outgoing frame counter is the field whose loss breaks a network, and
        // reporting a placeholder here is what made it look persisted when it
        // was not.
        let key_info = self
            .connection()?
            .command(GetNetworkKeyInfo)
            .await
            .map_err(|e| context("cannot read the network key info", &e))?;
        check("reading the network key info", key_info.status)?;

        Ok(NetworkInfo {
            pan_id: params.pan_id,
            // The extended PAN id is an eight-byte identifier in the same
            // little-endian wire order as an EUI64, not an address, so it goes
            // through the same conversion rather than a separate one.
            extended_pan_id: Ieee::from_be_bytes(params.extended_pan_id).raw(),
            channel: params.radio_channel,
            nwk_update_id: params.nwk_update_id,
            key_sequence: key_info.network_key_sequence_number,
            frame_counter: key_info.network_key_frame_counter,
        })
    }

    async fn network_key(&mut self) -> Result<Option<SecretKey>, AdapterError> {
        // EmberZNet does export the network key, which the runtime's own
        // comment used to claim it would not. Without it a stored network
        // describes itself but cannot be recreated on replacement hardware,
        // which is the entire point of storing it.
        match self
            .connection()?
            .command(ExportKey {
                context: SecurityManContext::network_key(),
            })
            .await
        {
            // A refusal is an answer, not a failure: firmware built without key
            // export says so here, and a caller that treats it as an error
            // cannot start at all on that build. That applies to a non-success
            // status just as much as to a transport failure.
            Ok(response) if response.status.is_ok() => {
                Ok(Some(SecretKey::new(*response.key.expose())))
            }
            Ok(response) => {
                warn!(
                    "the coordinator would not export its network key: {}",
                    response.status
                );
                Ok(None)
            }
            Err(e) => {
                warn!("the coordinator would not export its network key: {e}");
                Ok(None)
            }
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            // Not yet: backup means reading NVM3 tokens, and a wrong restore
            // is the most destructive operation available. Claiming support
            // before it is verified would let the runtime write a bad backup.
            backup: false,
            interpan: false,
            install_codes: false,
            // EZSP handles several concurrent APS requests; conservative until
            // measured under load.
            max_concurrent: 4,
            zdo_sequence_in_payload: true,
            manufacturer: SILABS,
        }
    }

    async fn permit_join(
        &mut self,
        duration: Duration,
        via: Option<Nwk>,
    ) -> Result<(), AdapterError> {
        if via.is_some() {
            // Opening joining through a specific router is a ZDO broadcast to
            // that router, not an EZSP call. It belongs in the runtime, which
            // owns ZDO.
            return Err(AdapterError::Unsupported(
                "permit join via a specific router (send Mgmt_Permit_Joining_req instead)",
            ));
        }
        // EZSP takes seconds as u8; 255 means "forever", which is a footgun we
        // do not expose, so clamp to 254.
        let secs = u8::try_from(duration.as_secs()).unwrap_or(254).min(254);

        // The transient key has to be in place *before* the window opens, or a
        // device that joins immediately finds no key to commission against.
        if secs == 0 {
            bringup::clear_commissioning_key(self.connection()?).await?;
        } else {
            bringup::install_commissioning_key(self.connection()?).await?;
        }

        let response = self
            .connection()?
            .command(PermitJoining { duration: secs })
            .await
            .map_err(|e| context("cannot open the join window", &e))?;
        check("opening the join window", response.status)?;
        info!(seconds = secs, "permit join");
        Ok(())
    }

    async fn send_zcl(&mut self, request: ZclTx) -> Result<Option<ZclRx>, AdapterError> {
        let tag = self.tag.next();
        let aps = EmberAdapter::aps_frame_for(&request, 0);
        // The EZSP payload type is a heapless vec capped at 255 bytes whose
        // `FromIterator` *panics* on overflow, and its name is not exported so
        // it cannot be constructed fallibly from here. Checking the length
        // first is what makes the `collect` below provably infallible -- an
        // over-long frame must be a typed error, never a panic on a data path.
        if request.frame.len() > MAX_APS_PAYLOAD {
            return Err(AdapterError::Transport(format!(
                "ZCL frame is {} bytes; EZSP accepts at most {MAX_APS_PAYLOAD}. \
                 APS fragmentation is not implemented yet.",
                request.frame.len()
            )));
        }

        // The payload is collected inline in each branch rather than once into
        // a binding, for the reason above: the type cannot be named here.
        match request.dest {
            Destination::Unicast { nwk, .. } => {
                let response = self
                    .connection()?
                    .command(SendUnicast {
                        unicast_type: UnicastType::Direct,
                        index_or_destination: nwk.raw(),
                        aps_frame: aps,
                        message_tag: tag.into(),
                        message: request.frame.clone(),
                    })
                    .await
                    .map_err(|e| context("cannot send the ZCL frame", &e))?;
                check("sending the ZCL frame", response.status)?;
            }
            Destination::Group(_) => {
                let response = self
                    .connection()?
                    .command(SendMulticast {
                        aps_frame: aps,
                        hops: GROUP_HOPS,
                        nonmember_radius: NONMEMBER_RADIUS,
                        message_tag: tag.into(),
                        message: request.frame.clone(),
                    })
                    .await
                    .map_err(|e| context("cannot send the group ZCL frame", &e))?;
                check("sending the group ZCL frame", response.status)?;
            }
            Destination::Broadcast(address) => {
                let response = self
                    .connection()?
                    .command(SendBroadcast {
                        destination: NodeId(address.to_nwk().raw()),
                        aps_frame: aps,
                        radius: BROADCAST_RADIUS,
                        message_tag: tag.into(),
                        message: request.frame.clone(),
                    })
                    .await
                    .map_err(|e| context("cannot broadcast the ZCL frame", &e))?;
                check("broadcasting the ZCL frame", response.status)?;
            }
        }

        // See the module docs: EZSP confirms delivery at the APS layer and
        // reports the device's reply separately, so there is no reply to return
        // here. The runtime correlates it from AdapterEvent::Zcl by ZCL TSN.
        Ok(None)
    }

    async fn send_zdo(&mut self, request: ZdoTx) -> Result<Option<Vec<u8>>, AdapterError> {
        let Destination::Unicast { nwk, .. } = request.dest else {
            return Err(AdapterError::Unsupported(
                "broadcast ZDO (implemented with the network map)",
            ));
        };

        if request.payload.len() > MAX_APS_PAYLOAD {
            return Err(AdapterError::Transport(format!(
                "ZDO payload is {} bytes; EZSP accepts at most {MAX_APS_PAYLOAD}",
                request.payload.len()
            )));
        }

        // ZDO is ordinary APS traffic: profile 0x0000, endpoint 0 both ways,
        // cluster = the ZDO cluster id. The transaction sequence number is the
        // first payload byte and is the caller's, because it is what the
        // caller matches the response against.
        let aps = ApsFrame {
            profile_id: 0x0000,
            cluster_id: request.cluster.0,
            source_endpoint: 0,
            destination_endpoint: 0,
            options: ApsOptions::RETRY.union(ApsOptions::ENABLE_ROUTE_DISCOVERY),
            group_id: 0,
            sequence: 0,
        };
        let tag = self.tag.next();

        let response = self
            .connection()?
            .command(SendUnicast {
                unicast_type: UnicastType::Direct,
                index_or_destination: nwk.raw(),
                aps_frame: aps,
                message_tag: tag.into(),
                message: request.payload.clone(),
            })
            .await
            .map_err(|e| context("cannot send the ZDO request", &e))?;
        check("sending the ZDO request", response.status)?;

        // Same as send_zcl: EZSP confirms at the APS layer and delivers the
        // response separately. The runtime matches it from AdapterEvent::Zdo
        // by the sequence number it put in the payload.
        Ok(None)
    }
}

/// Drains EZSP callbacks and translates the ones the runtime acts on.
///
/// This is the boundary. `rsezsp` hands over what the NCP said, decoded but not
/// interpreted -- `device_update` is a raw byte there because what that byte
/// *means* is a Zigbee question, not a transport one. Deciding it here is what
/// keeps the driver free of opinions about devices.
async fn pump_callbacks(
    mut callbacks: tokio::sync::mpsc::Receiver<Callback>,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
) {
    /// `EmberDeviceUpdate::DEVICE_LEFT`.
    ///
    /// A departure arrives on the same callback as an arrival, distinguished
    /// only by this byte. Treating every one of them as a join would resurrect
    /// devices that had just left.
    const DEVICE_LEFT: u8 = 0x02;

    /// ZDO traffic travels on profile 0.
    ///
    /// Handing a ZDO frame to a ZCL decoder produces confident nonsense, so the
    /// split happens here, where the profile is still visible.
    const ZDO_PROFILE: u16 = 0x0000;

    while let Some(cb) = callbacks.recv().await {
        let translated = match &cb {
            // A device appearing on, or leaving, the network. Handled here
            // because until it was, the Ember adapter never emitted
            // `DeviceJoined` at all: the callback was logged as unhandled and
            // dropped, so a device that joined stayed invisible to the runtime
            // until it happened to send a ZCL frame of its own. Everything the
            // runtime does on a join -- creating the record, interviewing,
            // resolving a definition, configuring reporting -- could therefore
            // never trigger against real hardware.
            Callback::TrustCenterJoin {
                node_id,
                eui64,
                device_update,
                ..
            } => {
                let ieee = eui64_to_ieee(*eui64);
                let nwk = Nwk::new(node_id.0);
                if *device_update == DEVICE_LEFT {
                    info!(%ieee, "device left the network");
                    Some(AdapterEvent::DeviceLeft {
                        ieee: Some(ieee),
                        nwk: Some(nwk),
                    })
                } else {
                    info!(%ieee, nwk = nwk.raw(), device_update, "device joined");
                    Some(AdapterEvent::DeviceJoined {
                        ieee: Some(ieee),
                        nwk,
                    })
                }
            }
            Callback::IncomingMessage {
                aps_frame,
                sender,
                payload,
                last_hop_lqi,
                ..
            } if aps_frame.profile_id == ZDO_PROFILE => Some(AdapterEvent::Zdo {
                cluster: rszigbee_spec::zdo::ZdoClusterId(aps_frame.cluster_id),
                nwk: Nwk::new(sender.0),
                payload: payload.clone(),
            })
            .inspect(|_| {
                debug!(lqi = last_hop_lqi, "ZDO frame");
            }),
            Callback::IncomingMessage {
                aps_frame,
                sender,
                payload,
                last_hop_lqi,
                ..
            } => Some(AdapterEvent::Zcl(ZclRx {
                // EZSP reports the short address; the IEEE is the runtime's to
                // resolve, and inventing one here would be a guess.
                ieee: None,
                nwk: Nwk::new(sender.0),
                endpoint: EndpointId(aps_frame.source_endpoint),
                destination_endpoint: EndpointId(aps_frame.destination_endpoint),
                cluster: ClusterId(aps_frame.cluster_id),
                group: None,
                was_broadcast: false,
                link_quality: Some(*last_hop_lqi),
                frame: payload.clone(),
            })),
            other => {
                // Everything else is logged rather than dropped silently: the
                // set of callbacks that matter grows with each phase, and a
                // quiet discard is how a missing one stays missing.
                debug!(callback = ?other, "unhandled EZSP callback");
                None
            }
        };

        if let Some(event) = translated
            && events.send(event).await.is_err()
        {
            debug!("event receiver dropped; stopping callback pump");
            return;
        }
    }
    warn!("EZSP callback channel closed");
}

/// Converts an EZSP EUI64 into an [`Ieee`].
fn eui64_to_ieee(eui: Eui64) -> Ieee {
    // EUI64 renders big-endian in text but is little-endian on the wire; go via
    // the bytes rather than the string form so no parsing is involved.
    // `to_wire` is little-endian, which is the order the NCP uses. An IEEE
    // address is written big-endian, so the bytes reverse.
    let mut bytes = eui.to_wire();
    bytes.reverse();
    Ieee::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rszigbee_adapter::TxOptions;

    #[test]
    fn a_joined_coordinator_resumes() {
        for state in [
            NetworkStatus::JOINED_NETWORK,
            NetworkStatus::JOINED_NETWORK_NO_PARENT,
        ] {
            assert_eq!(
                EmberAdapter::outcome_for(state, MismatchPolicy::Fail).unwrap(),
                StartOutcome::Resumed,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_blank_coordinator_refuses_to_form_by_default() {
        // The most destructive thing this crate can do. Forming a network on a
        // coordinator that has none orphans every device the user owns, so the
        // default must be to stop and explain.
        let e = EmberAdapter::outcome_for(NetworkStatus::NO_NETWORK, MismatchPolicy::Fail)
            .expect_err("must refuse");
        assert!(matches!(e, AdapterError::NetworkMismatch(_)));
        assert!(
            e.to_string().contains("MismatchPolicy::Form"),
            "must say how to opt in"
        );
    }

    #[test]
    fn forming_is_only_reachable_by_explicit_opt_in() {
        assert_eq!(
            EmberAdapter::outcome_for(NetworkStatus::NO_NETWORK, MismatchPolicy::Form).unwrap(),
            StartOutcome::Formed
        );
    }

    #[test]
    fn a_transient_network_state_is_refused_rather_than_raced() {
        for state in [NetworkStatus::JOINING, NetworkStatus::LEAVING_NETWORK] {
            let e = EmberAdapter::outcome_for(state, MismatchPolicy::Form)
                .expect_err("must refuse mid-transition");
            assert!(e.to_string().contains("transient"), "{state:?} -> {e}");
        }
    }

    #[test]
    fn a_group_frame_carries_the_group_id_and_no_retry() {
        // The group id goes in the APS frame, because `sendMulticast` takes no
        // address and reads it from there. Send it with the group unset and
        // every multicast addresses group zero.
        let req = ZclTx::group(
            rszigbee_spec::ids::GroupId(0x1234),
            EndpointId(1),
            ClusterId(0x0006),
            vec![0x01, 0x07, 0x01],
        );
        let aps = EmberAdapter::aps_frame_for(&req, 0);
        assert_eq!(aps.group_id, 0x1234, "the group must reach the APS frame");

        // And no retry. A multicast is never acknowledged -- there is no single
        // recipient to acknowledge it -- so asking makes every send wait out a
        // timeout and then report a delivery failure that did not happen.
        let opts = aps.options.0;
        assert_eq!(
            opts & ApsOptions::RETRY.0,
            0,
            "APS retry on a multicast waits for an ack that cannot come"
        );
    }

    #[test]
    fn a_broadcast_frame_carries_neither_retry_nor_route_discovery() {
        let req = ZclTx::broadcast(
            rszigbee_adapter::BroadcastAddress::All,
            EndpointId(1),
            ClusterId(0x0006),
            vec![0x01, 0x07, 0x01],
        );
        let aps = EmberAdapter::aps_frame_for(&req, 0);
        let opts = aps.options.0;
        assert_eq!(
            opts & ApsOptions::RETRY.0,
            0,
            "a broadcast is not acknowledged"
        );
        assert_eq!(
            opts & ApsOptions::ENABLE_ROUTE_DISCOVERY.0,
            0,
            "a broadcast has no route to discover"
        );
        assert_eq!(aps.group_id, 0, "a broadcast is not a group");
    }

    #[test]
    fn aps_frames_carry_retry_and_route_discovery() {
        // Without RETRY a single lost frame looks like an unreachable device;
        // without route discovery the first message to a device behind a router
        // fails. Every working stack sets both.
        let req = ZclTx::unicast(
            Ieee::new(1),
            Nwk::new(0x1234),
            EndpointId(1),
            ClusterId(0x0006),
            vec![0x01, 0x07, 0x01],
        );
        let aps = EmberAdapter::aps_frame_for(&req, 0);
        let opts = aps.options.0;
        assert_ne!(opts & ApsOptions::RETRY.0, 0, "RETRY must be set");
        assert_ne!(
            opts & ApsOptions::ENABLE_ROUTE_DISCOVERY.0,
            0,
            "route discovery must be set"
        );
        assert_eq!(aps.cluster_id, 0x0006);
        assert_eq!(aps.profile_id, 0x0104);
        assert_eq!(aps.destination_endpoint, 1);
    }

    #[test]
    fn a_probe_skips_route_discovery() {
        // An availability probe that triggers route repair turns a cheap check
        // into an expensive one and distorts what it is measuring.
        let req = ZclTx::unicast(
            Ieee::new(1),
            Nwk::new(2),
            EndpointId(1),
            ClusterId(0x0000),
            vec![],
        )
        .with_options(TxOptions::probe(Duration::from_secs(2)));
        let aps = EmberAdapter::aps_frame_for(&req, 0);
        assert_eq!(aps.options.0 & ApsOptions::ENABLE_ROUTE_DISCOVERY.0, 0);
        assert_ne!(aps.options.0 & ApsOptions::RETRY.0, 0);
    }

    #[test]
    fn message_tags_advance_and_wrap_without_panicking() {
        let mut t = MessageTag::default();
        assert_eq!(t.next(), 1);
        assert_eq!(t.next(), 2);
        let mut t = MessageTag(255);
        assert_eq!(t.next(), 0, "must wrap, not overflow");
    }

    #[test]
    fn the_builder_takes_settings_from_the_fingerprint_table() {
        let path = "/dev/serial/by-id/usb-Itead_Sonoff_Zigbee_3.0_USB_Dongle_Plus_V2_x-if00-port0";
        let (a, _rx) = EmberAdapter::serial(path).build();
        assert_eq!(
            a.settings,
            SerialSettings {
                baud: 115_200,
                rtscts: false
            }
        );
    }

    #[test]
    fn explicit_settings_override_the_table() {
        let (a, _rx) = EmberAdapter::serial("/dev/ttyUSB0")
            .baud(230_400)
            .rtscts(true)
            .build();
        assert_eq!(
            a.settings,
            SerialSettings {
                baud: 230_400,
                rtscts: true
            }
        );
    }

    #[test]
    fn capabilities_do_not_claim_unverified_support() {
        // Claiming backup before it is verified would let the runtime write a
        // backup it cannot restore, which is worse than having none.
        let (a, _rx) = EmberAdapter::serial("/dev/ttyUSB0").build();
        let c = a.capabilities();
        assert!(!c.backup);
        assert!(!c.interpan);
        assert_eq!(c.manufacturer, SILABS);
    }

    #[tokio::test]
    async fn every_command_fails_cleanly_before_start() {
        let (mut a, _rx) = EmberAdapter::serial("/dev/ttyUSB0").build();
        assert!(matches!(
            a.coordinator_ieee().await,
            Err(AdapterError::NotConnected)
        ));
        assert!(matches!(
            a.network_info().await,
            Err(AdapterError::NotConnected)
        ));
        assert!(matches!(
            a.permit_join(Duration::from_secs(60), None).await,
            Err(AdapterError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn permit_join_via_a_router_is_refused_with_a_pointer() {
        // Not an EZSP call at all; saying so is more useful than a generic error.
        let (mut a, _rx) = EmberAdapter::serial("/dev/ttyUSB0").build();
        let e = a
            .permit_join(Duration::from_secs(60), Some(Nwk::new(0x1234)))
            .await
            .expect_err("must refuse");
        assert!(e.to_string().contains("Mgmt_Permit_Joining_req"));
    }
}
