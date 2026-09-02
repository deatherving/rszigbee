//! A scriptable in-memory adapter.
//!
//! This is not a test convenience bolted on afterwards — it is compiled
//! unconditionally and is the primary way the runtime is tested.
//! Hardware-independent testing is a hard requirement: `cargo test --workspace`
//! must pass with no dongle, no broker and no Node.
//!
//! The mock also serves as the reference implementation of the trait. If
//! something is awkward to express here, the trait is probably wrong — which is
//! exactly the falsification the first vertical slice is looking for.

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rszigbee_spec::ids::{ClusterId, Ieee, ManufacturerCode, Nwk};

use crate::error::{AdapterError, DisconnectReason};
use crate::tx::{ZclRx, ZclTx, ZdoTx};
use crate::{
    AdapterCapabilities, AdapterEvent, CoordinatorAdapter, FirmwareInfo, NetworkConfig,
    NetworkInfo, StartOutcome,
};

/// One scripted reply.
enum Reply {
    Zcl(Result<Option<ZclRx>, AdapterError>),
    Zdo(Result<Option<Vec<u8>>, AdapterError>),
}

#[derive(Default)]
struct Shared {
    started: bool,
    zcl_sent: Vec<ZclTx>,
    zdo_sent: Vec<ZdoTx>,
    replies: VecDeque<Reply>,
    permit_join: Vec<(Duration, Option<Nwk>)>,
    outcome: Option<StartOutcome>,
    start_error: Option<String>,
}

/// Control side of a [`MockAdapter`], usable from a test after the adapter has
/// been handed to the runtime.
#[derive(Clone)]
pub struct MockHandle {
    shared: Arc<Mutex<Shared>>,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
}

impl core::fmt::Debug for MockHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MockHandle")
    }
}

impl MockHandle {
    /// Delivers an event as though the coordinator had reported it.
    ///
    /// This is how a test drives the runtime: joins, leaves, incoming frames
    /// and a dropped link all reach the runtime through the same channel a real
    /// adapter uses, so nothing under test knows it is not talking to hardware.
    ///
    /// Returns `false` if the receiver is gone or its buffer is full. A full
    /// buffer is deliberately not an error here for the same reason it is not
    /// one in a real adapter: the channel is bounded, and a slow consumer must
    /// drop rather than grow.
    pub fn emit(&self, event: AdapterEvent) -> bool {
        self.events.try_send(event).is_ok()
    }

    /// Queues the next `send_zcl` result.
    pub fn reply_zcl(&self, r: Result<Option<ZclRx>, AdapterError>) {
        if let Ok(mut s) = self.shared.lock() {
            s.replies.push_back(Reply::Zcl(r));
        }
    }

    /// Queues the next `send_zdo` result.
    pub fn reply_zdo(&self, r: Result<Option<Vec<u8>>, AdapterError>) {
        if let Ok(mut s) = self.shared.lock() {
            s.replies.push_back(Reply::Zdo(r));
        }
    }

    /// Makes `start` report this outcome.
    pub fn set_start_outcome(&self, outcome: StartOutcome) {
        if let Ok(mut s) = self.shared.lock() {
            s.outcome = Some(outcome);
        }
    }

    /// Makes `start` fail with a network mismatch.
    pub fn fail_start(&self, reason: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.start_error = Some(reason.to_string());
        }
    }

    /// Pushes an event as if the coordinator had reported it.
    pub async fn inject(&self, event: AdapterEvent) {
        let _ = self.events.send(event).await;
    }

    /// Every ZCL request sent so far.
    #[must_use]
    pub fn zcl_sent(&self) -> Vec<ZclTx> {
        self.shared
            .lock()
            .map(|s| s.zcl_sent.clone())
            .unwrap_or_default()
    }

    /// Every ZDO request sent so far.
    #[must_use]
    pub fn zdo_sent(&self) -> Vec<ZdoTx> {
        self.shared
            .lock()
            .map(|s| s.zdo_sent.clone())
            .unwrap_or_default()
    }

    /// Every `permit_join` call so far.
    #[must_use]
    pub fn permit_join_calls(&self) -> Vec<(Duration, Option<Nwk>)> {
        self.shared
            .lock()
            .map(|s| s.permit_join.clone())
            .unwrap_or_default()
    }

    /// True once `start` has succeeded and `stop` has not run.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.shared.lock().is_ok_and(|s| s.started)
    }
}

/// An in-memory [`CoordinatorAdapter`].
pub struct MockAdapter {
    shared: Arc<Mutex<Shared>>,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
    ieee: Ieee,
    caps: AdapterCapabilities,
}

impl core::fmt::Debug for MockAdapter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MockAdapter")
    }
}

impl MockAdapter {
    /// Creates an adapter, its control handle and the event receiver.
    #[must_use]
    pub fn new() -> (Self, MockHandle, tokio::sync::mpsc::Receiver<AdapterEvent>) {
        // Bounded on purpose. An unbounded event channel turns a slow consumer
        // into unbounded memory growth. A full channel must drop with a counter,
        // never grow.
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let me = Self {
            shared: Arc::clone(&shared),
            events: tx.clone(),
            ieee: Ieee::new(0x0012_4b00_2218_9abc),
            caps: AdapterCapabilities {
                backup: true,
                install_codes: true,
                max_concurrent: 2,
                manufacturer: ManufacturerCode(0x1049),
                ..AdapterCapabilities::default()
            },
        };
        (me, MockHandle { shared, events: tx }, rx)
    }

    /// Overrides the coordinator address.
    #[must_use]
    pub const fn with_ieee(mut self, ieee: Ieee) -> Self {
        self.ieee = ieee;
        self
    }

    /// Overrides the reported capabilities.
    #[must_use]
    pub const fn with_capabilities(mut self, caps: AdapterCapabilities) -> Self {
        self.caps = caps;
        self
    }

    fn require_started(&self) -> Result<(), AdapterError> {
        let started = self.shared.lock().is_ok_and(|s| s.started);
        if started {
            Ok(())
        } else {
            Err(AdapterError::NotConnected)
        }
    }
}

// Every method is async because `CoordinatorAdapter` says so -- a real adapter
// awaits a serial port. A synchronous fake has nothing to await and still has
// to match the signature, so the lint has no fix to offer that would not make
// the mock worse to read.
#[allow(clippy::unused_async_trait_impl)]
impl CoordinatorAdapter for MockAdapter {
    async fn start(
        &mut self,
        network: &NetworkConfig,
        _backup: Option<&[u8]>,
    ) -> Result<StartOutcome, AdapterError> {
        let _ = network;
        let mut s = self.shared.lock().map_err(|_| AdapterError::NotConnected)?;
        if let Some(reason) = s.start_error.take() {
            return Err(AdapterError::NetworkMismatch(reason));
        }
        s.started = true;
        Ok(s.outcome.unwrap_or(StartOutcome::Resumed))
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        if let Ok(mut s) = self.shared.lock() {
            s.started = false;
        }
        let _ = self
            .events
            .send(AdapterEvent::Disconnected(DisconnectReason::Requested))
            .await;
        Ok(())
    }

    async fn coordinator_ieee(&mut self) -> Result<Ieee, AdapterError> {
        self.require_started()?;
        Ok(self.ieee)
    }

    async fn firmware(&mut self) -> Result<FirmwareInfo, AdapterError> {
        self.require_started()?;
        Ok(FirmwareInfo {
            family: "mock".into(),
            version: "0.0.1".into(),
            meta: Vec::new(),
        })
    }

    async fn network_info(&mut self) -> Result<NetworkInfo, AdapterError> {
        self.require_started()?;
        Ok(NetworkInfo {
            pan_id: 0x1a62,
            extended_pan_id: 0xdddd_dddd_dddd_dddd,
            channel: 11,
            nwk_update_id: 0,
        })
    }

    fn capabilities(&self) -> AdapterCapabilities {
        self.caps
    }

    async fn permit_join(
        &mut self,
        duration: Duration,
        via: Option<Nwk>,
    ) -> Result<(), AdapterError> {
        self.require_started()?;
        if let Ok(mut s) = self.shared.lock() {
            s.permit_join.push((duration, via));
        }
        Ok(())
    }

    async fn send_zcl(&mut self, request: ZclTx) -> Result<Option<ZclRx>, AdapterError> {
        self.require_started()?;
        let reply = {
            let mut s = self.shared.lock().map_err(|_| AdapterError::NotConnected)?;
            s.zcl_sent.push(request.clone());
            s.replies.pop_front()
        };
        match reply {
            Some(Reply::Zcl(r)) => r,
            // A ZDO reply queued for a ZCL send is a test bug, and saying so is
            // more useful than silently returning None.
            Some(Reply::Zdo(_)) => Err(AdapterError::Transport(
                "mock: next queued reply was for send_zdo, not send_zcl".into(),
            )),
            None if request.options.expect_response => Ok(None),
            None => Ok(None),
        }
    }

    async fn send_zdo(&mut self, request: ZdoTx) -> Result<Option<Vec<u8>>, AdapterError> {
        self.require_started()?;
        let reply = {
            let mut s = self.shared.lock().map_err(|_| AdapterError::NotConnected)?;
            s.zdo_sent.push(request.clone());
            s.replies.pop_front()
        };
        match reply {
            Some(Reply::Zdo(r)) => r,
            Some(Reply::Zcl(_)) => Err(AdapterError::Transport(
                "mock: next queued reply was for send_zcl, not send_zdo".into(),
            )),
            None => Ok(None),
        }
    }

    async fn backup(&mut self, _known: &[Ieee]) -> Result<Vec<u8>, AdapterError> {
        self.require_started()?;
        Ok(b"{\"metadata\":{\"format\":\"zigpy/open-coordinator-backup\",\"version\":1}}".to_vec())
    }
}

/// A convenience constructor for a plausible attribute report, used by tests
/// across the workspace.
#[must_use]
pub fn zcl_rx(nwk: Nwk, cluster: ClusterId, frame: Vec<u8>) -> ZclRx {
    ZclRx {
        ieee: None,
        nwk,
        endpoint: rszigbee_spec::ids::EndpointId(1),
        destination_endpoint: rszigbee_spec::ids::EndpointId(1),
        cluster,
        group: None,
        was_broadcast: false,
        link_quality: Some(120),
        frame,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::TxOptions;
    use rszigbee_spec::ids::EndpointId;

    fn config() -> NetworkConfig {
        NetworkConfig {
            pan_id: Some(0x1a62),
            extended_pan_id: None,
            channel: 11,
            network_key: None,
            on_mismatch: crate::MismatchPolicy::Fail,
        }
    }

    #[tokio::test]
    async fn nothing_works_before_start() {
        // A runtime bug that calls the adapter too early should surface as a
        // typed error, not as a plausible-looking fake answer.
        let (mut a, _h, _rx) = MockAdapter::new();
        assert!(matches!(
            a.coordinator_ieee().await,
            Err(AdapterError::NotConnected)
        ));
        assert!(matches!(
            a.permit_join(Duration::from_secs(60), None).await,
            Err(AdapterError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn start_resumes_by_default() {
        let (mut a, h, _rx) = MockAdapter::new();
        assert_eq!(
            a.start(&config(), None).await.unwrap(),
            StartOutcome::Resumed
        );
        assert!(h.is_started());
    }

    #[tokio::test]
    async fn a_network_mismatch_is_reported_not_papered_over() {
        let (mut a, h, _rx) = MockAdapter::new();
        h.fail_start("channel 15 on coordinator, 11 configured");
        let err = a.start(&config(), None).await.unwrap_err();
        assert!(matches!(err, AdapterError::NetworkMismatch(_)));
        assert!(!h.is_started());
    }

    #[tokio::test]
    async fn requests_are_recorded_for_assertions() {
        let (mut a, h, _rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();

        let tx = ZclTx::unicast(
            Ieee::new(1),
            Nwk::new(0x1234),
            EndpointId(1),
            ClusterId(0x0006),
            vec![0x01, 0x07, 0x01],
        )
        .with_options(TxOptions::no_response());
        a.send_zcl(tx).await.unwrap();

        let sent = h.zcl_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent.first().map(|t| t.cluster), Some(ClusterId(0x0006)));
        // The frame bytes are the ones the caller built: the mock must not
        // reinterpret or rewrite them.
        assert_eq!(
            sent.first().map(|t| t.frame.clone()),
            Some(vec![0x01, 0x07, 0x01])
        );
    }

    #[tokio::test]
    async fn scripted_replies_are_returned_in_order() {
        let (mut a, h, _rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();

        h.reply_zcl(Ok(Some(zcl_rx(
            Nwk::new(1),
            ClusterId(0x0006),
            vec![0x18, 0x01, 0x0b],
        ))));
        h.reply_zcl(Err(AdapterError::Tx(crate::TxFailure::NoAck)));

        let first = a
            .send_zcl(ZclTx::unicast(
                Ieee::new(1),
                Nwk::new(1),
                EndpointId(1),
                ClusterId(0x0006),
                vec![],
            ))
            .await
            .unwrap();
        assert!(first.is_some());

        let second = a
            .send_zcl(ZclTx::unicast(
                Ieee::new(1),
                Nwk::new(1),
                EndpointId(1),
                ClusterId(0x0006),
                vec![],
            ))
            .await;
        assert!(matches!(
            second,
            Err(AdapterError::Tx(crate::TxFailure::NoAck))
        ));
    }

    #[tokio::test]
    async fn a_misqueued_reply_is_an_explicit_error() {
        let (mut a, h, _rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();
        h.reply_zdo(Ok(None));
        let r = a
            .send_zcl(ZclTx::unicast(
                Ieee::new(1),
                Nwk::new(1),
                EndpointId(1),
                ClusterId(0x0006),
                vec![],
            ))
            .await;
        assert!(matches!(r, Err(AdapterError::Transport(_))));
    }

    #[tokio::test]
    async fn injected_events_reach_the_receiver() {
        let (mut a, h, mut rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();
        h.inject(AdapterEvent::DeviceJoined {
            ieee: Some(Ieee::new(9)),
            nwk: Nwk::new(0xabcd),
        })
        .await;
        let ev = rx.recv().await.expect("event");
        assert_eq!(
            ev,
            AdapterEvent::DeviceJoined {
                ieee: Some(Ieee::new(9)),
                nwk: Nwk::new(0xabcd)
            }
        );
    }

    #[tokio::test]
    async fn stop_reports_the_disconnect_so_the_runtime_can_react() {
        let (mut a, h, mut rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();
        a.stop().await.unwrap();
        assert!(!h.is_started());
        assert_eq!(
            rx.recv().await,
            Some(AdapterEvent::Disconnected(DisconnectReason::Requested))
        );
    }

    #[tokio::test]
    async fn permit_join_records_duration_and_router() {
        let (mut a, h, _rx) = MockAdapter::new();
        a.start(&config(), None).await.unwrap();
        a.permit_join(Duration::from_secs(60), None).await.unwrap();
        a.permit_join(Duration::ZERO, Some(Nwk::new(0x1234)))
            .await
            .unwrap();
        assert_eq!(
            h.permit_join_calls(),
            vec![
                (Duration::from_secs(60), None),
                (Duration::ZERO, Some(Nwk::new(0x1234)))
            ]
        );
    }

    #[tokio::test]
    async fn the_default_trait_method_reports_unsupported_rather_than_panicking() {
        // Install codes are supported by the mock, so use a fresh adapter that
        // declares them unsupported to exercise the default body.
        struct Bare;
        // Same as the mock: the signatures come from the trait, and this stub
        // exists precisely to have nothing in its bodies.
        #[allow(clippy::unused_async_trait_impl)]
        impl CoordinatorAdapter for Bare {
            async fn start(
                &mut self,
                _n: &NetworkConfig,
                _b: Option<&[u8]>,
            ) -> Result<StartOutcome, AdapterError> {
                Ok(StartOutcome::Resumed)
            }
            async fn stop(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn coordinator_ieee(&mut self) -> Result<Ieee, AdapterError> {
                Ok(Ieee::ZERO)
            }
            async fn firmware(&mut self) -> Result<FirmwareInfo, AdapterError> {
                Ok(FirmwareInfo {
                    family: "bare".into(),
                    version: "0".into(),
                    meta: Vec::new(),
                })
            }
            async fn network_info(&mut self) -> Result<NetworkInfo, AdapterError> {
                Ok(NetworkInfo {
                    pan_id: 0,
                    extended_pan_id: 0,
                    channel: 11,
                    nwk_update_id: 0,
                })
            }
            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities::default()
            }
            async fn permit_join(
                &mut self,
                _d: Duration,
                _v: Option<Nwk>,
            ) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn send_zcl(&mut self, _r: ZclTx) -> Result<Option<ZclRx>, AdapterError> {
                Ok(None)
            }
            async fn send_zdo(&mut self, _r: ZdoTx) -> Result<Option<Vec<u8>>, AdapterError> {
                Ok(None)
            }
        }

        let mut b = Bare;
        let err = b.backup(&[]).await.unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Unsupported("coordinator backup")
        ));
        let err = b.add_install_code(Ieee::ZERO, &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported("install codes")));
    }
}
