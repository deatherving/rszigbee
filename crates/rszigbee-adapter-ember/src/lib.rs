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
pub mod fingerprint;
pub mod form;
mod session;

use std::time::Duration;

use ezsp::ember::aps::{Frame as ApsFrame, Options as ApsOptions};
use ezsp::ember::message::Destination as EmberDestination;
use ezsp::ember::network::Status as NetworkStatus;
use ezsp::{Configuration, Messaging, Networking, Utilities};
use rszigbee_adapter::{
    AdapterCapabilities, AdapterError, AdapterEvent, CoordinatorAdapter, Destination, FirmwareInfo,
    MismatchPolicy, NetworkConfig, NetworkInfo, StartOutcome, ZclRx, ZclTx, ZdoTx,
};
use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, ManufacturerCode, Nwk};
use tracing::{debug, info, warn};

pub use fingerprint::{SerialSettings, recognise, settings_for};

/// The largest APS payload EZSP will accept in one frame. Beyond this the
/// stack needs APS fragmentation, which is not implemented yet.
const MAX_APS_PAYLOAD: usize = 255;

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

    fn connection(&mut self) -> Result<&mut ezsp::Connection, AdapterError> {
        self.session
            .as_mut()
            .map(|s| &mut s.connection)
            .ok_or(AdapterError::NotConnected)
    }

    /// Builds the APS frame for a ZCL request.
    ///
    /// `RETRY` and `ENABLE_ROUTE_DISCOVERY` are the options every working stack
    /// sets for a unicast: without retry a single lost frame looks like an
    /// unreachable device, and without route discovery the first message to a
    /// device behind a router fails.
    fn aps_frame_for(req: &ZclTx, sequence: u8) -> ApsFrame {
        let mut options = ApsOptions::RETRY;
        if !req.options.disable_recovery {
            options |= ApsOptions::ENABLE_ROUTE_DISCOVERY;
        }
        ApsFrame::new(
            req.profile.0,
            req.cluster.0,
            req.source_endpoint.0,
            req.endpoint.0,
            options,
            0,
            sequence,
        )
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
            NetworkStatus::JoinedNetwork | NetworkStatus::JoinedNetworkNoParent => {
                Ok(StartOutcome::Resumed)
            }
            NetworkStatus::NoNetwork => match policy {
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
                "coordinator is in a transient network state ({other:?}); retry shortly"
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
        let mut session = session::connect(&self.path, self.settings, session::VERSIONS).await?;

        let eui = session
            .connection
            .get_eui64()
            .await
            .map_err(|e| map_ezsp(&e))?;
        let coordinator = eui64_to_ieee(eui);

        // Order matters and is not arbitrary. EZSP rejects `addEndpoint` once
        // the stack is running, so endpoints and identity must be configured
        // before the network comes up.
        bringup::configure_endpoints(&mut session.connection, SILABS).await?;

        // Stack configuration, before the network comes up: EZSP refuses these
        // writes once it is running. Stack profile, security level and
        // end-device capacity are advertised in every beacon, and a device
        // whose scan reads the wrong value never attempts to associate -- so
        // getting these wrong produces silence, not an error.
        bringup::configure_stack(&mut session.connection).await?;

        // Trust-centre policies, also before the stack comes up. `permitJoining`
        // only opens the association window; whether a device is *admitted* is
        // a separate decision, and the EmberZNet default admits only devices
        // with a preconfigured key -- which no ordinary device has. Without
        // this, joining opened and nothing ever joined.
        bringup::configure_join_policies(&mut session.connection).await?;

        // Then resume any stored network. Skipping this makes `networkState`
        // report NoNetwork on a coordinator that has a perfectly good network,
        // which would lead a stack straight into forming over it. Observed on
        // real hardware.
        let stored = bringup::resume_stored_network(&mut session.connection).await?;

        let state = session
            .connection
            .network_state()
            .await
            .map_err(|e| map_ezsp(&e))?;

        info!(
            ezsp = session.version,
            coordinator = %coordinator,
            ?stored,
            ?state,
            "Ember coordinator online"
        );

        let outcome = EmberAdapter::outcome_for(state, network.on_mismatch)?;
        if matches!(outcome, StartOutcome::Formed) {
            let formed = form::form(&mut session.connection, coordinator, network).await?;
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
            .get_eui64()
            .await
            .map_err(|e| map_ezsp(&e))?;
        let ieee = eui64_to_ieee(eui);
        self.coordinator = Some(ieee);
        Ok(ieee)
    }

    async fn firmware(&mut self) -> Result<FirmwareInfo, AdapterError> {
        let ezsp_version = self.session.as_ref().map_or(0, |s| s.version);
        let raw = self
            .connection()?
            .get_value(ezsp::ezsp::value::Id::VersionInfo)
            .await
            .map_err(|e| map_ezsp(&e))?;

        // VERSION_INFO is build (u16 LE), major, minor, patch, special, type.
        let bytes: Vec<u8> = raw.iter().copied().collect();
        let version = match bytes.as_slice() {
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
        let (_node_type, params) = self
            .connection()?
            .get_network_parameters()
            .await
            .map_err(|e| map_ezsp(&e))?;
        Ok(NetworkInfo {
            pan_id: params.pan_id(),
            extended_pan_id: eui64_to_ieee(params.extended_pan_id()).raw(),
            channel: params.radio_channel(),
            nwk_update_id: params.nwk_update_id(),
        })
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

        self.connection()?
            .permit_joining(secs.into())
            .await
            .map_err(|e| map_ezsp(&e))?;
        info!(seconds = secs, "permit join");
        Ok(())
    }

    async fn send_zcl(&mut self, request: ZclTx) -> Result<Option<ZclRx>, AdapterError> {
        let Destination::Unicast { nwk, .. } = request.dest else {
            return Err(AdapterError::Unsupported(
                "group and broadcast ZCL (implemented in a later phase)",
            ));
        };

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

        self.connection()?
            .send_unicast(
                EmberDestination::Direct(ezsp::ember::NodeId::from(nwk.raw())),
                aps,
                tag,
                request.frame.iter().copied().collect(),
            )
            .await
            .map_err(|e| map_ezsp(&e))?;

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
        let aps = ApsFrame::new(
            0x0000,
            request.cluster.0,
            0,
            0,
            ApsOptions::RETRY | ApsOptions::ENABLE_ROUTE_DISCOVERY,
            0,
            0,
        );
        let tag = self.tag.next();

        self.connection()?
            .send_unicast(
                EmberDestination::Direct(ezsp::ember::NodeId::from(nwk.raw())),
                aps,
                tag,
                request.payload.iter().copied().collect(),
            )
            .await
            .map_err(|e| map_ezsp(&e))?;

        // Same as send_zcl: EZSP confirms at the APS layer and delivers the
        // response separately. The runtime matches it from AdapterEvent::Zdo
        // by the sequence number it put in the payload.
        Ok(None)
    }
}

/// Drains EZSP callbacks and translates the ones the runtime acts on.
async fn pump_callbacks(
    mut callbacks: tokio::sync::mpsc::Receiver<ezsp::Callback>,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
) {
    use ezsp::ember::device::Update as DeviceUpdate;
    use ezsp::parameters::messaging::handler::Handler as Messaging;
    use ezsp::parameters::trust_center::handler::Handler as TrustCenter;

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
            ezsp::Callback::TrustCenter(TrustCenter::TrustCenterJoin(j)) => {
                let ieee = eui64_to_ieee(j.new_node_eui64());
                let nwk = Nwk::new(j.new_node_id());
                match j.status() {
                    // A departure is reported by the same callback as an
                    // arrival, distinguished only by this status. Treating
                    // every one of them as a join would resurrect devices that
                    // had just left.
                    Ok(DeviceUpdate::DeviceLeft) => {
                        info!(%ieee, "device left the network");
                        Some(AdapterEvent::DeviceLeft {
                            ieee: Some(ieee),
                            nwk: Some(nwk),
                        })
                    }
                    Ok(update) => {
                        info!(%ieee, nwk = nwk.raw(), ?update, "device joined");
                        Some(AdapterEvent::DeviceJoined {
                            ieee: Some(ieee),
                            nwk,
                        })
                    }
                    // A status this build does not model. Reported as a join
                    // rather than dropped: the callback only fires for a
                    // device whose membership changed, and a device the
                    // runtime knows about is recoverable while one it never
                    // heard of is not.
                    Err(raw) => {
                        warn!(%ieee, raw, "unmodelled trust-centre join status");
                        Some(AdapterEvent::DeviceJoined {
                            ieee: Some(ieee),
                            nwk,
                        })
                    }
                }
            }
            ezsp::Callback::Messaging(Messaging::IncomingMessage(m))
                if m.aps_frame().profile_id() == 0x0000 =>
            {
                // Profile 0 is ZDO, not ZCL. Handing a ZDO frame to a ZCL
                // decoder produces confident nonsense, so the split happens
                // here where the profile is still visible.
                let aps = m.aps_frame();
                Some(AdapterEvent::Zdo {
                    cluster: rszigbee_spec::zdo::ZdoClusterId(aps.cluster_id()),
                    nwk: Nwk::new(m.sender()),
                    payload: m.message().to_vec(),
                })
            }
            ezsp::Callback::Messaging(Messaging::IncomingMessage(m)) => {
                let aps = m.aps_frame();
                Some(AdapterEvent::Zcl(ZclRx {
                    // EZSP reports the short address; the IEEE is the runtime's
                    // to resolve, and inventing one here would be a guess.
                    ieee: None,
                    nwk: Nwk::new(m.sender()),
                    endpoint: EndpointId(aps.source_endpoint()),
                    destination_endpoint: EndpointId(aps.destination_endpoint()),
                    cluster: ClusterId(aps.cluster_id()),
                    group: None,
                    was_broadcast: false,
                    link_quality: Some(m.last_hop_lqi()),
                    frame: m.message().to_vec(),
                }))
            }
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

/// Maps an EZSP error into the adapter's error type.
fn map_ezsp(e: &ezsp::Error) -> AdapterError {
    AdapterError::Transport(e.to_string())
}

/// Converts an EZSP EUI64 into an [`Ieee`].
fn eui64_to_ieee(eui: ezsp::ember::Eui64) -> Ieee {
    // EUI64 renders big-endian in text but is little-endian on the wire; go via
    // the byte array rather than the string form so no parsing is involved.
    Ieee::from_be_bytes(eui.into_array())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rszigbee_adapter::TxOptions;

    #[test]
    fn a_joined_coordinator_resumes() {
        for state in [
            NetworkStatus::JoinedNetwork,
            NetworkStatus::JoinedNetworkNoParent,
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
        let e = EmberAdapter::outcome_for(NetworkStatus::NoNetwork, MismatchPolicy::Fail)
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
            EmberAdapter::outcome_for(NetworkStatus::NoNetwork, MismatchPolicy::Form).unwrap(),
            StartOutcome::Formed
        );
    }

    #[test]
    fn a_transient_network_state_is_refused_rather_than_raced() {
        for state in [NetworkStatus::JoiningNetwork, NetworkStatus::LeavingNetwork] {
            let e = EmberAdapter::outcome_for(state, MismatchPolicy::Form)
                .expect_err("must refuse mid-transition");
            assert!(e.to_string().contains("transient"), "{state:?} -> {e}");
        }
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
        let opts = u16::from(aps.options());
        assert_ne!(opts & u16::from(ApsOptions::RETRY), 0, "RETRY must be set");
        assert_ne!(
            opts & u16::from(ApsOptions::ENABLE_ROUTE_DISCOVERY),
            0,
            "route discovery must be set"
        );
        assert_eq!(aps.cluster_id(), 0x0006);
        assert_eq!(aps.profile_id(), 0x0104);
        assert_eq!(aps.destination_endpoint(), 1);
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
        assert_eq!(
            u16::from(aps.options()) & u16::from(ApsOptions::ENABLE_ROUTE_DISCOVERY),
            0
        );
        assert_ne!(u16::from(aps.options()) & u16::from(ApsOptions::RETRY), 0);
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
