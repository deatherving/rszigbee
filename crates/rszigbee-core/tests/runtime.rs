//! The runtime, exercised through its public API.
//!
//! These live in `tests/` rather than beside the code deliberately. Everything
//! here reaches the runtime the way an application does — `Zigbee::builder`, a
//! handle, an event stream — so if this file compiles, the public API is
//! sufficient to drive and observe the runtime. As a `#[cfg(test)]` module it
//! could have reached into private internals without anyone noticing, and the
//! surface would have looked complete while being unusable from outside.
//!
//! Nothing here needs hardware: `MockAdapter` stands in for a coordinator.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod tests {
    use std::time::Duration;

    use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, EndpointId, Ieee, Nwk};
    use rszigbee_spec::zdo::ZdoClusterId;

    use rszigbee_core::adapter::StartOutcome;
    use rszigbee_core::command::CommandError;
    use rszigbee_core::runtime::{EventStream, RuntimeError, Zigbee};
    use rszigbee_spec::zcl::ZclValue;

    use rszigbee_core::adapter::{AdapterEvent, MockAdapter, MockHandle, ZclRx};
    use rszigbee_core::command::{DeviceCommand, ZclAttributeWrite, ZclCommand};
    use rszigbee_core::device::InterviewState;
    use rszigbee_core::event::{Event, ZclMessageKind};
    use rszigbee_core::store::{MemoryStore, PersistedNetwork, ZigbeeStore};

    /// A joined device. Deliberately *not* the coordinator's address: these
    /// two used to be the same value, which was only harmless while the
    /// coordinator had no device record of its own.
    const DEVICE: Ieee = Ieee::new(0x0012_4b00_2218_9abd);
    /// What `MockAdapter` reports as its own address.
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
    async fn an_injected_reachability_policy_is_the_one_consulted() {
        // `ReachabilityPolicy` is documented as one of the four extension
        // points, and `reachability_policy` is the only way to reach it. An
        // extension point with no test is an extension point that might not
        // be wired up at all — this is the test that says it is.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use rszigbee_core::reachability::{
            Assessment, NextCheck, Reachability, ReachabilityContext, ReachabilityPolicy,
        };

        struct Counting(Arc<AtomicUsize>);
        impl ReachabilityPolicy for Counting {
            fn assess(&self, _ctx: &ReachabilityContext<'_>) -> Assessment {
                self.0.fetch_add(1, Ordering::Relaxed);
                Assessment {
                    // A verdict the default policy would not give for a device
                    // that has only just been heard from, so the event proves
                    // *this* policy produced it.
                    verdict: Reachability::Unreachable,
                    next: NextCheck::AwaitTraffic,
                }
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, MemoryStore::new())
            .reachability_policy(Counting(Arc::clone(&calls)))
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        control.emit(AdapterEvent::DeviceJoined {
            ieee: Some(Ieee::new(0x0012_4b00_2218_9abc)),
            nwk: Nwk::new(0x1234),
        });

        let verdict = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::ReachabilityChanged { to, .. }) => return to,
                    Some(_) => {}
                    None => panic!("the stream closed"),
                }
            }
        })
        .await
        .expect("the injected policy should be asked");

        assert_eq!(
            verdict,
            Reachability::Unreachable,
            "the runtime must use the injected policy, not its default"
        );
        assert!(calls.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    async fn the_coordinator_is_a_device_like_any_other() {
        // It sits at nwk 0x0000, hosts genBasic and answers ZDO. Without a
        // record, `devices()` omits the one device an operator is certain
        // exists and every request to it is `UnknownDevice`.
        let (zigbee, _control) = runtime().await;
        let devices = zigbee.devices().await.expect("devices");
        let coordinator = devices
            .iter()
            .find(|d| d.ieee == zigbee.coordinator())
            .expect("the coordinator must be in the device table");
        assert_eq!(coordinator.nwk, Nwk::COORDINATOR);
        assert_eq!(
            coordinator.kind,
            rszigbee_core::device::DeviceKind::Coordinator
        );
        // Mains by definition, which also keeps the availability policy from
        // ever probing it.
        assert_eq!(
            coordinator.power_source,
            rszigbee_core::device::PowerSource::Mains
        );
    }

    #[tokio::test]
    async fn a_request_addressed_to_the_coordinator_is_not_refused() {
        // The gap this closes: before the coordinator had a record, reading
        // its own genBasic through the runtime failed with `UnknownDevice`.
        let (zigbee, control) = runtime().await;
        control.reply_zcl(Ok(None));
        let ieee = zigbee.coordinator();
        // Times out rather than erroring, because the mock does not answer —
        // what matters is that it was *sent* rather than refused.
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            zigbee.zcl_read(ieee, EndpointId(1), ClusterId(0x0000), vec![AttrId(0x0005)]),
        )
        .await;
        assert!(
            result.is_err() || result.is_ok(),
            "placeholder to keep the await"
        );
        assert_eq!(
            control.zcl_sent().len(),
            1,
            "the read must reach the adapter rather than being refused"
        );
    }

    #[tokio::test]
    async fn starting_reports_what_it_did_and_who_the_coordinator_is() {
        let (zigbee, _control) = runtime().await;
        assert_eq!(zigbee.coordinator(), COORDINATOR);
        // Resumed, not formed. Forming when we should have resumed is the
        // outcome that orphans every device.
        assert_eq!(zigbee.start_outcome(), StartOutcome::Resumed);
        // The coordinator itself, and nothing else: no device has joined.
        let devices = zigbee.devices().await.expect("devices");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].ieee, zigbee.coordinator());
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
        let joined = devices
            .iter()
            .find(|d| d.ieee == DEVICE)
            .expect("the joined device is in the table");
        // Unknown, not guessed. Guessing "router" would make a battery sensor
        // get probed forever.
        assert_eq!(joined.kind, rszigbee_core::device::DeviceKind::Unknown);
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

        // Still one record for this device, not two: a short address is not an
        // identity. The coordinator's own record is the other entry.
        let devices = zigbee.devices().await.expect("devices");
        assert_eq!(
            devices.iter().filter(|d| d.ieee == DEVICE).count(),
            1,
            "a rejoin must update the record, not add one: {devices:?}"
        );
        assert_eq!(
            devices.iter().find(|d| d.ieee == DEVICE).map(|d| d.nwk),
            Some(Nwk::new(0x2222))
        );
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
        let mut stored = rszigbee_core::store::PersistedDevice::new(DEVICE, Nwk::new(0x4321));
        stored.interview = InterviewState::Successful;
        store.upsert_device(&stored).await.expect("upsert");

        let (adapter, _control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, store)
            .start()
            .await
            .expect("start");

        let devices = zigbee.devices().await.expect("devices");
        let restored = devices
            .iter()
            .find(|d| d.ieee == DEVICE)
            .expect("the stored device is in the table");
        assert_eq!(restored.nwk, Nwk::new(0x4321));
        // Restored as already interviewed, so a restart does not re-interview
        // every device on the network.
        assert_eq!(restored.interview, InterviewState::Successful);
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
mod definition_integration {
    use std::time::Duration;

    use rszigbee_devices::{Definition, DefinitionIndex, Extend};
    use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, Ieee, Nwk};
    use rszigbee_spec::zdo::ZdoClusterId;

    use rszigbee_core::command::CommandError;
    use rszigbee_core::runtime::{RuntimeError, Zigbee};

    use rszigbee_core::adapter::{AdapterEvent, MockAdapter, MockHandle, ZclRx};
    use rszigbee_core::command::DeviceCommand;
    use rszigbee_core::device::{DeviceKind, InterviewState};
    use rszigbee_core::event::Event;
    use rszigbee_core::store::{MemoryStore, PersistedDevice, PersistedEndpoint, ZigbeeStore};

    const BULB: Ieee = Ieee::new(0x0017_8801_00dc_4d3f);
    /// A joined sensor, distinct from the coordinator's own address.
    const SENSOR: Ieee = Ieee::new(0x0012_4b00_2218_9abd);

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

        // On/off from the explicit binding, brightness implied by the `Light`
        // capability. A dimmable light that reports its state but not its
        // brightness shows the wrong level after anyone uses the wall switch.
        assert!(
            plan.iter().any(|s| s.cluster == ClusterId(0x0006)),
            "on/off reporting must be planned: {plan:?}"
        );
        assert!(
            plan.iter().any(|s| s.cluster == ClusterId(0x0008)),
            "brightness reporting must be planned: {plan:?}"
        );
        assert!(plan.iter().all(|s| s.endpoint == EndpointId(1)));
        assert!(
            plan.iter().all(|s| s.max_interval > 0),
            "without a max interval a silent device is indistinguishable from a dead one"
        );
        // The explicit binding and the implied `state` source name the same
        // attribute, and configuring it twice is meaningless.
        let on_off: Vec<_> = plan
            .iter()
            .filter(|s| s.cluster == ClusterId(0x0006))
            .collect();
        assert_eq!(on_off.len(), 1, "duplicate reporting config: {on_off:?}");
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

    #[tokio::test]
    async fn a_custom_cluster_is_registered_so_its_frames_decode() {
        // Without the registration a manufacturer-specific cluster's
        // attributes have no known types, so a frame from it decodes to
        // nothing and the device looks like it reports rubbish.
        let mut custom = rszigbee_devices::CustomCluster::default();
        custom.name = "boschEnergyDevice".into();
        custom.id = ClusterId(0xfca0);
        custom.manufacturer = Some(0x1209);
        // 0x30 is enum8.
        custom.attributes = vec![(0x0001, "switchType".into(), 0x30)];
        // A *cluster-specific* response, which is what actually requires the
        // registration: an attribute report is a global command decoded by the
        // type on the wire, so it would decode either way. Only a
        // cluster-specific frame has to be looked up by cluster, and an
        // unregistered one fails with `UnknownCluster`.
        custom.responses = vec![(
            0x00,
            "alarmState".into(),
            vec![("state".to_owned(), 0x20u8)],
        )];

        let mut definition = Definition::new("BMCT-RZ");
        definition.match_rules.models = vec!["RBSH-MMR-ZB-EU".into()];
        definition.extend = vec![
            Extend::AddCustomCluster(custom),
            Extend::Temperature(rszigbee_devices::NumericSpec::default()),
        ];
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(
            adapter,
            events,
            stored(SENSOR, "RBSH-MMR-ZB-EU", &[0x0000, 0xfca0]).await,
        )
        .definitions(index)
        .interview_on_join(false)
        .start()
        .await
        .expect("start");
        let mut stream = zigbee.events();

        // Frame control 0x09: cluster-specific, server to client. Command
        // 0x00 with one uint8 parameter.
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xfca0),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame: vec![0x09, 0x09, 0x00, 0x02],
        }));

        let name = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::ZclMessage(m)) if m.cluster == ClusterId(0xfca0) => match m.kind {
                        rszigbee_core::event::ZclMessageKind::Command { name, params, .. } => {
                            assert_eq!(params.len(), 1, "the declared parameter decodes");
                            return name;
                        }
                        other => panic!("expected a command, got {other:?}"),
                    },
                    Some(Event::UnparsedFrame {
                        cluster: ClusterId(0xfca0),
                        reason,
                        ..
                    }) => panic!("a registered custom cluster must decode, got {reason:?}"),
                    Some(_) => {}
                    None => panic!("the stream closed"),
                }
            }
        })
        .await
        .expect("an event should arrive");
        assert_eq!(
            name.as_deref(),
            Some("alarmState"),
            "the command name comes from the registered custom cluster"
        );
    }

    // ---- the sensor path: bind, configure reporting, report, StateChanged

    /// Drives an interview to completion against a scripted mock, so the
    /// configure plan actually executes.
    ///
    /// The mock answers every ZDO request with `Ok(None)` and every ZCL send
    /// with `Ok(None)`, which is what the Ember adapter does; the interview
    /// steps then time out, but resolution still happens from the store's
    /// model string, which is what this exercises.
    #[tokio::test]
    async fn binding_and_reporting_are_configured_for_a_recognised_sensor() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(
            adapter,
            events,
            stored(SENSOR, "TS0601", &[0x0000, 0x0402]).await,
        )
        .definitions(index())
        .interview_on_join(false)
        .start()
        .await
        .expect("start");

        // The plan is what execution follows, so assert on it first.
        let plan = zigbee.configure_plan(SENSOR).await.expect("plan");
        assert!(
            plan.iter().any(|s| s.cluster == ClusterId(0x0402)),
            "a temperature capability must imply temperature reporting: {plan:?}"
        );

        // Now run it. No replies are queued: the mock's default is `Ok(None)`
        // for both send paths, which is what the Ember adapter does. Queueing
        // them would interleave two kinds in one queue and the mock rightly
        // rejects a mismatch.
        let outcome = zigbee.configure(SENSOR).await.expect("configure");
        assert!(outcome.bound > 0, "at least one binding: {outcome:?}");
        assert!(
            outcome.configured > 0,
            "at least one attribute: {outcome:?}"
        );

        // A bind for the temperature cluster...
        let zdo = control.zdo_sent();
        let bind = zdo
            .iter()
            .find(|tx| tx.cluster == ZdoClusterId::BIND_REQ)
            .expect("a Bind_req must be sent, or reports have nowhere to go");
        // sequence, source IEEE, source endpoint, cluster
        // sequence(1) + source IEEE(8) + source endpoint(1) + cluster(2)
        // + address mode(1) + destination IEEE(8) + destination endpoint(1)
        assert_eq!(bind.payload.len(), 22, "{:?}", bind.payload);
        let cluster = u16::from_le_bytes([bind.payload[10], bind.payload[11]]);
        assert_eq!(cluster, 0x0402);
        // Address mode 3: a 64-bit address plus an endpoint.
        assert_eq!(bind.payload[12], 0x03);

        // ...and a configureReporting for its attribute.
        let zcl = control.zcl_sent();
        let configure = zcl
            .iter()
            .find(|tx| tx.cluster == ClusterId(0x0402) && tx.frame.get(2) == Some(&0x06))
            .expect("configureReporting must be sent, or the device reports only on poll");
        // direction, attribute, type, min, max
        assert_eq!(configure.frame[3], 0x00, "direction: reported to us");
        let attribute = u16::from_le_bytes([configure.frame[4], configure.frame[5]]);
        assert_eq!(attribute, 0x0000);
        let max = u16::from_le_bytes([configure.frame[8], configure.frame[9]]);
        assert!(max > 0, "a zero max interval hides a dead device");
    }

    #[tokio::test]
    async fn a_temperature_report_becomes_a_typed_state_change() {
        // The whole sensor path in one test: the definition says this device
        // has a temperature, a report arrives, and a caller sees 21.37 rather
        // than cluster 0x0402 attribute 0x0000 = 2137.
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(
            adapter,
            events,
            stored(SENSOR, "TS0601", &[0x0000, 0x0402]).await,
        )
        .definitions(index())
        .interview_on_join(false)
        .start()
        .await
        .expect("start");
        let mut stream = zigbee.events();

        // msTemperatureMeasurement.measuredValue = 2137, an int16.
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0402),
            group: None,
            was_broadcast: false,
            link_quality: Some(180),
            frame: vec![0x18, 0x07, 0x0a, 0x00, 0x00, 0x29, 0x59, 0x08],
        }));

        let changes = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::StateChanged { changes, .. }) => return changes,
                    Some(_) => {}
                    None => panic!("the stream closed before a state change"),
                }
            }
        })
        .await
        .expect("a modelled attribute must produce a state change");

        let value = changes
            .get(&rszigbee_core::capability::CapabilityId::from(
                "temperature",
            ))
            .expect("the capability the definition names");
        assert_eq!(
            value.as_f64(),
            Some(21.37),
            "the divisor is implied by the capability, not stated by the definition"
        );
    }

    #[tokio::test]
    async fn an_unmodelled_attribute_produces_no_state_change_but_still_an_event() {
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(
            adapter,
            events,
            stored(SENSOR, "TS0601", &[0x0000, 0x0402]).await,
        )
        .definitions(index())
        .interview_on_join(false)
        .start()
        .await
        .expect("start");
        let mut stream = zigbee.events();

        // Cluster the definition says nothing about.
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0405),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame: vec![0x18, 0x08, 0x0a, 0x00, 0x00, 0x21, 0x10, 0x27],
        }));

        // A raw event, yes; invented state, no.
        let mut saw_raw = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(40), stream.recv()).await {
                Ok(Some(Event::StateChanged { changes, .. })) => {
                    panic!("an unmodelled attribute must not invent state: {changes:?}")
                }
                Ok(Some(Event::ZclMessage(_))) => saw_raw = true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            saw_raw,
            "the frame must still surface, or nobody can model the attribute"
        );
    }

    // ---- the Tuya path, end to end

    /// A Tuya soil sensor: temperature and moisture on datapoints.
    fn tuya_definition() -> Definition {
        let mut d = Definition::new("TS0601_soil");
        d.match_rules.fingerprints = vec![{
            let mut fp = rszigbee_devices::Fingerprint::default();
            fp.model_id = Some("TS0601".into());
            fp.manufacturer_name = Some("_TZE200_myd45weu".into());
            fp
        }];
        d.extend = vec![Extend::TuyaBase {
            datapoints: true,
            query_on_announce: false,
            query_interval_secs: None,
        }];
        let mut tenths = rszigbee_devices::NumericSpec::default();
        tenths.divisor = 10;
        d.tuya_datapoints = vec![
            rszigbee_devices::TuyaDatapoint::new(
                3,
                "soil_moisture",
                rszigbee_devices::TuyaKind::Value(rszigbee_devices::NumericSpec::default()),
            ),
            rszigbee_devices::TuyaDatapoint::new(
                5,
                "temperature",
                rszigbee_devices::TuyaKind::Value(tenths),
            ),
            rszigbee_devices::TuyaDatapoint::new(
                1,
                "state",
                rszigbee_devices::TuyaKind::Bool { inverted: false },
            )
            .writable(),
        ];
        d
    }

    /// A store holding a Tuya device that has been interviewed.
    async fn tuya_store() -> MemoryStore {
        let store = MemoryStore::new();
        let mut device = PersistedDevice::new(SENSOR, Nwk::new(0x1234));
        device.interview = InterviewState::Successful;
        device.basic.model_id = Some("TS0601".into());
        device.basic.manufacturer_name = Some("_TZE200_myd45weu".into());
        device.endpoints = vec![PersistedEndpoint {
            id: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            device_id: 0x0051,
            input_clusters: vec![ClusterId(0x0000), ClusterId(0xef00)],
            output_clusters: Vec::new(),
        }];
        store.upsert_device(&device).await.expect("upsert");
        store
    }

    async fn tuya_runtime() -> (Zigbee, MockHandle) {
        let mut index = DefinitionIndex::new();
        index.insert(tuya_definition()).expect("insert");
        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, tuya_store().await)
            .definitions(index)
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        (zigbee, control)
    }

    #[tokio::test]
    async fn a_tuya_datapoint_report_becomes_typed_state() {
        // The whole Tuya read path: a manufacturer-cluster frame carrying two
        // datapoints becomes two scaled capabilities. Nothing in the standard
        // attribute path would ever see this frame.
        let (zigbee, control) = tuya_runtime().await;
        let mut stream = zigbee.events();

        // dataReport (command 0x02) with dp 5 = 213 tenths and dp 3 = 42.
        let mut payload = vec![0x00, 0x01];
        payload.extend_from_slice(&[0x05, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0xd5]);
        payload.extend_from_slice(&[0x03, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x2a]);
        let mut frame = vec![0x09, 0x11, 0x02];
        frame.extend_from_slice(&payload);

        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            group: None,
            was_broadcast: false,
            link_quality: Some(160),
            frame,
        }));

        let changes = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::StateChanged { changes, .. }) => return changes,
                    Some(_) => {}
                    None => panic!("the stream closed"),
                }
            }
        })
        .await
        .expect("a Tuya report must produce state");

        assert_eq!(
            changes
                .get(&rszigbee_core::capability::CapabilityId::from(
                    "temperature"
                ))
                .and_then(rszigbee_core::state::StateValue::as_f64),
            Some(21.3),
            "the table's divisor is what makes 213 mean 21.3"
        );
        assert_eq!(
            changes
                .get(&rszigbee_core::capability::CapabilityId::from(
                    "soil_moisture"
                ))
                .and_then(rszigbee_core::state::StateValue::as_f64),
            Some(42.0)
        );
    }

    #[tokio::test]
    async fn a_command_to_a_tuya_device_becomes_a_data_request() {
        let (zigbee, control) = tuya_runtime().await;
        control.reply_zcl(Ok(None));

        zigbee
            .send(SENSOR, DeviceCommand::SetOn(true))
            .await
            .expect("the table declares a writable state datapoint");

        let sent = control.zcl_sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0].cluster,
            ClusterId(0xef00),
            "a Tuya device is addressed through its manufacturer cluster, not genOnOff"
        );
        // Frame: control, tsn, command 0x00 (dataRequest), then seq and the
        // datapoint. dp 1, type 1 (bool), length 1, value 1.
        assert_eq!(sent[0].frame.get(2), Some(&0x00), "dataRequest");
        let payload = &sent[0].frame[3..];
        assert_eq!(payload[2], 0x01, "datapoint 1");
        assert_eq!(payload[3], 0x01, "bool");
        assert_eq!(payload[6], 0x01, "true");
    }

    #[tokio::test]
    async fn an_unmodelled_datapoint_produces_no_state() {
        // Guessing would attach the value to whatever capability shared its
        // number, and the result reads like a plausible measurement.
        let (zigbee, control) = tuya_runtime().await;
        let mut stream = zigbee.events();

        // dp 99, which the table does not name.
        let mut frame = vec![0x09, 0x12, 0x02, 0x00, 0x01];
        frame.extend_from_slice(&[0x63, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01]);
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));

        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(40), stream.recv()).await {
                Ok(Some(Event::StateChanged { changes, .. })) => {
                    panic!("an unmodelled datapoint must not invent state: {changes:?}")
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    #[tokio::test]
    async fn a_delegated_datapoint_is_handled_by_its_named_behaviour() {
        // The escape hatch end to end: one datapoint delegated to Rust, the
        // rest of the device still declarative.
        let mut definition = Definition::new("TS0601_thermostat");
        definition.match_rules.fingerprints = vec![{
            let mut fp = rszigbee_devices::Fingerprint::default();
            fp.model_id = Some("TS0601".into());
            fp.manufacturer_name = Some("_TZE200_thermostat".into());
            fp
        }];
        definition.extend = vec![Extend::TuyaBase {
            datapoints: true,
            query_on_announce: false,
            query_interval_secs: None,
        }];
        definition.tuya_datapoints = vec![
            rszigbee_devices::TuyaDatapoint::new(
                28,
                "schedule_monday",
                rszigbee_devices::TuyaKind::Behavior {
                    name: "tuya:thermostat-schedule".into(),
                },
            )
            .writable(),
            rszigbee_devices::TuyaDatapoint::new(
                2,
                "current_heating_setpoint",
                rszigbee_devices::TuyaKind::Value({
                    let mut spec = rszigbee_devices::NumericSpec::default();
                    spec.divisor = 10;
                    spec
                }),
            ),
        ];
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let store = MemoryStore::new();
        let mut device = PersistedDevice::new(SENSOR, Nwk::new(0x1234));
        device.interview = InterviewState::Successful;
        device.basic.model_id = Some("TS0601".into());
        device.basic.manufacturer_name = Some("_TZE200_thermostat".into());
        device.endpoints = vec![PersistedEndpoint {
            id: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            device_id: 0x0051,
            input_clusters: vec![ClusterId(0x0000), ClusterId(0xef00)],
            output_clusters: Vec::new(),
        }];
        store.upsert_device(&device).await.expect("upsert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, store)
            .definitions(index)
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        // dp 28, raw: Monday with two transitions.
        let schedule = [1u8, 6, 0, 0, 210, 8, 0, 0, 170];
        let mut frame = vec![0x09, 0x20, 0x02, 0x00, 0x01, 28, 0x00];
        #[expect(clippy::cast_possible_truncation, reason = "test data, nine bytes")]
        let length = schedule.len() as u16;
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&schedule);

        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));

        let changes = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::StateChanged { changes, .. }) => return changes,
                    Some(_) => {}
                    None => panic!("the stream closed"),
                }
            }
        })
        .await
        .expect("the behaviour must produce state");

        assert_eq!(
            changes
                .get(&rszigbee_core::capability::CapabilityId::from(
                    "schedule_monday"
                ))
                .cloned(),
            Some(rszigbee_core::state::StateValue::Str(
                "06:00/21.0 08:00/17.0".into()
            )),
            "a table could not express this, and a named behaviour did"
        );
    }

    #[tokio::test]
    async fn a_caller_supplied_behaviour_is_consulted_for_its_datapoint() {
        // `ZigbeeBuilder::behavior` is the only way to add behaviour for a
        // device nobody has contributed a definition for. Until this test it
        // had no caller anywhere, which means it could have been wired up
        // wrongly and nothing would have said so.
        use rszigbee_core::state::StateChanges;
        use rszigbee_core::{DecodeContext, DeviceBehavior, Outcome};

        struct Doubling;
        impl DeviceBehavior for Doubling {
            fn name(&self) -> &'static str {
                "test:doubling"
            }
            fn decode_datapoint(&self, ctx: &DecodeContext<'_>) -> Outcome<StateChanges> {
                let rszigbee_spec::tuya::Value::Number(raw) = ctx.datapoint.value else {
                    return Outcome::NotHandled;
                };
                let mut changes = StateChanges::new();
                changes.set(
                    rszigbee_core::CapabilityId::from(ctx.capability),
                    rszigbee_core::StateValue::Int(i64::from(raw) * 2),
                );
                Outcome::Handled(changes)
            }
        }

        let mut definition = Definition::new("TS0601_custom");
        definition.match_rules.models = vec!["TS0601".into()];
        definition.tuya_datapoints = vec![rszigbee_devices::TuyaDatapoint::new(
            7,
            "odd_reading",
            rszigbee_devices::TuyaKind::Behavior {
                name: "test:doubling".into(),
            },
        )];
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, tuya_store().await)
            .definitions(index)
            .behavior(Doubling)
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        // dp 7, a four-byte number: 21.
        let mut frame = vec![0x09, 0x30, 0x02, 0x00, 0x01, 7, 0x02, 0x00, 0x04];
        frame.extend_from_slice(&21i32.to_be_bytes());
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));

        let changes = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.recv().await {
                    Some(Event::StateChanged { changes, .. }) => return changes,
                    Some(_) => {}
                    None => panic!("the stream closed"),
                }
            }
        })
        .await
        .expect("the caller's behaviour should be consulted");

        assert_eq!(
            changes.get(&rszigbee_core::CapabilityId::from("odd_reading")),
            Some(&rszigbee_core::StateValue::Int(42)),
            "the value came from the caller's own behaviour, not a default"
        );
    }

    #[tokio::test]
    async fn a_datapoint_naming_a_behaviour_nothing_implements_produces_nothing() {
        // Not a fallback and not a crash: the datapoint is simply unhandled,
        // and the coverage report is where that shows up.
        let mut definition = Definition::new("TS0601_unknown");
        definition.match_rules.models = vec!["TS0601".into()];
        definition.tuya_datapoints = vec![rszigbee_devices::TuyaDatapoint::new(
            9,
            "mystery",
            rszigbee_devices::TuyaKind::Behavior {
                name: "nobody:implements-this".into(),
            },
        )];
        let mut index = DefinitionIndex::new();
        index.insert(definition).expect("insert");

        let (adapter, control, events) = MockAdapter::new();
        let zigbee = Zigbee::builder(adapter, events, tuya_store().await)
            .definitions(index)
            .interview_on_join(false)
            .start()
            .await
            .expect("start");
        let mut stream = zigbee.events();

        let mut frame = vec![0x09, 0x21, 0x02, 0x00, 0x01, 9, 0x00, 0x00, 0x01, 0x07];
        frame.truncate(10);
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(SENSOR),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame,
        }));

        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(40), stream.recv()).await {
                Ok(Some(Event::StateChanged { changes, .. })) => {
                    panic!("an unimplemented behaviour must not fall back to a guess: {changes:?}")
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
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
    async fn an_echo_of_our_own_request_does_not_resolve_the_read() {
        // Found on hardware. EmberZNet loops a unicast addressed to the
        // coordinator's own node id back to the local application, so our own
        // `readAttributes` request arrived carrying our own sequence number and
        // resolved the read with nothing — in 27ms rather than the 5s timeout,
        // which is how it was noticed at all.
        //
        // The general case is worse than the loopback: the sequence is one
        // byte, so it wraps every 256 transactions and any unrelated frame
        // reusing one would resolve a pending read with whatever it contained.
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0000]).await).await;
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
        .expect("the read reaches the adapter");

        // Frame control 0x00: global, *client to server* — the outbound
        // direction. Command 0x00 is readAttributes, the request itself.
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(BULB),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0000),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame: vec![0x00, tsn, 0x00, 0x05, 0x00],
        }));

        // The read must still be waiting. A short window: the bug resolved it
        // within 30ms, so anything that returns here is the bug back.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut Box::pin(async {}))
                .await
                .is_ok()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !reader.is_finished(),
            "an outbound frame must not resolve a pending read"
        );

        // The real answer does resolve it. Frame control 0x18: global, server
        // to client. Command 0x01 is readAttributesResponse.
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

        let values = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("no timeout")
            .expect("task")
            .expect("the genuine response must resolve the read");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, 0x0005);
    }

    #[tokio::test]
    async fn a_device_refusing_a_read_is_reported_as_a_refusal_not_an_empty_result() {
        // "unsupported attribute" and "no values" are different things to a
        // caller: one is actionable, the other looks like a working read of
        // nothing.
        let (zigbee, control) =
            runtime_with(stored(BULB, "TRADFRI bulb E27 WS opal 980lm", &[0x0000]).await).await;
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

        // defaultResponse: server to client, command 0x0b, responding to
        // command 0x00 with status 0x86 (unsupported attribute).
        control.emit(AdapterEvent::Zcl(ZclRx {
            ieee: Some(BULB),
            nwk: Nwk::new(0x1234),
            endpoint: EndpointId(1),
            destination_endpoint: EndpointId(1),
            cluster: ClusterId(0x0000),
            group: None,
            was_broadcast: false,
            link_quality: None,
            frame: vec![0x18, tsn, 0x0b, 0x00, 0x86],
        }));

        let error = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("no timeout")
            .expect("task")
            .expect_err("a refusal is an error, not an empty read");
        assert!(
            matches!(error, RuntimeError::ReadRefused { status: 0x86, .. }),
            "{error:?}"
        );
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
