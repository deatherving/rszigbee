//! The runtime end to end, with no hardware.
//!
//! ```text
//! cargo run -p rszigbee --example runtime_mock
//! ```
//!
//! A mock coordinator stands in for a dongle, so this exercises the real
//! runtime — device table, address resolution, interview, persistence, event
//! stream — on any machine. That is not a shortcut for a demo: it is the same
//! path the test suite uses, and it is why `cargo test` needs no dongle, no
//! broker and no Node.
//!
//! What it does *not* prove is anything about a radio. The Ember adapter is
//! verified separately against real firmware by `ember_selftest`.

use std::time::Duration;

use rszigbee::adapter::{AdapterEvent, MockAdapter, ZclRx};
use rszigbee::spec::ids::{ClusterId, EndpointId, Nwk};
use rszigbee::{Event, Ieee, MemoryStore, Zigbee};

/// The device our fake coordinator will report.
const SENSOR: Ieee = Ieee::new(0x0012_4b00_2218_9abc);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();

    let (adapter, coordinator, adapter_events) = MockAdapter::new();

    // Interviewing is off because the mock has no ZDO responses scripted: a
    // real interview would time out, and waiting five seconds to demonstrate a
    // timeout is not the point here.
    let zigbee = Zigbee::builder(adapter, adapter_events, MemoryStore::new())
        .interview_on_join(false)
        .start()
        .await?;

    println!("coordinator: {}", zigbee.coordinator());
    println!("start:       {:?}", zigbee.start_outcome());

    let mut events = zigbee.events();

    // Opening joining goes through the runtime, which resolves a router's
    // permanent address to the short address the adapter wants.
    zigbee.permit_join(Duration::from_secs(60), None).await?;

    // ---- a device joins
    coordinator.emit(AdapterEvent::DeviceJoined {
        ieee: Some(SENSOR),
        nwk: Nwk::new(0x1234),
    });

    // ---- it reports a temperature: msTemperatureMeasurement.measuredValue,
    // 21.37 degrees, which ZCL carries as a signed hundredths value of 2137.
    coordinator.emit(AdapterEvent::Zcl(ZclRx {
        ieee: Some(SENSOR),
        nwk: Nwk::new(0x1234),
        endpoint: EndpointId(1),
        destination_endpoint: EndpointId(1),
        cluster: ClusterId(0x0402),
        group: None,
        was_broadcast: false,
        link_quality: Some(174),
        frame: vec![0x18, 0x07, 0x0a, 0x00, 0x00, 0x29, 0x59, 0x08],
    }));

    // ---- it rejoins with a different short address, which is the case that
    // breaks stacks that treat the short address as an identity.
    coordinator.emit(AdapterEvent::DeviceJoined {
        ieee: Some(SENSOR),
        nwk: Nwk::new(0x5678),
    });

    println!("\n=== events ===");
    let deadline = tokio::time::sleep(Duration::from_millis(500));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = events.recv() => match event {
                Some(Event::ZclMessage(message)) => {
                    println!("  {} {:?}", message.ieee, message.kind);
                }
                Some(event) => println!("  {event:?}"),
                None => break,
            },
        }
    }

    println!("\n=== devices ===");
    for device in zigbee.devices().await? {
        println!(
            "  {} nwk 0x{:04x}, {:?}, interview {:?}, lqi {:?}",
            device.ieee,
            device.nwk.raw(),
            device.kind,
            device.interview,
            device.link_quality
        );
    }

    // One device, at its new short address: a rejoin updated the record rather
    // than creating a second one.
    let devices = zigbee.devices().await?;
    assert_eq!(devices.len(), 1, "a rejoin must not create a second device");

    zigbee.stop().await?;
    println!("\nstopped cleanly");
    Ok(())
}
