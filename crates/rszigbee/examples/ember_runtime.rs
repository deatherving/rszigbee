//! The runtime against real hardware.
//!
//! ```text
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --form
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --permit-join
//! ```
//!
//! `ember_selftest` drives the *adapter* directly. This drives the runtime, and
//! that distinction is the point: the runtime, the ZCL transaction correlation,
//! the `genBasic` read and the device table have only ever been exercised
//! against `MockAdapter` — which is a model of how a coordinator behaves, and
//! this project has already found that model wrong several times (hardware flow
//! control hanging in the kernel, `networkInit` being mandatory, an
//! unregistered endpoint returning no endpoints, the adapter answering
//! `Ok(None)` instead of inline).
//!
//! # What it can prove without a second device
//!
//! The coordinator is itself a Zigbee node at `nwk 0x0000` with a `genBasic`
//! cluster, so it can be read and interviewed like anything else. That
//! exercises the correlation path end to end: a request goes out, the response
//! arrives asynchronously as an adapter event, and the runtime matches it to
//! the caller by transaction sequence.
//!
//! # What it cannot
//!
//! Binding, attribute reporting and inbound reports need a device that is not
//! the coordinator. Those stay unverified against hardware until one exists.
//!
//! # `--form` writes to the dongle
//!
//! Without it, a coordinator with no network makes this exit rather than
//! forming one. With it a **new network is created** and anything joined to a
//! previous one is orphaned. Safe on a blank dongle, destructive otherwise.

use std::time::Duration;

use rszigbee::adapter::{MismatchPolicy, NetworkConfig};
use rszigbee::ember::EmberAdapter;
use rszigbee::spec::ids::{AttrId, ClusterId, EndpointId};
use rszigbee::{Event, FileStore, Zigbee};

/// `genBasic`.
const GEN_BASIC: ClusterId = ClusterId(0x0000);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: ember_runtime <serial-path> [--form] [--permit-join]")?
        .clone();
    let may_form = args.iter().any(|a| a == "--form");
    let permit_join = args.iter().any(|a| a == "--permit-join");

    let store = FileStore::open(
        std::env::var("RSZIGBEE_DATA").unwrap_or_else(|_| "./rszigbee-data".into()),
    )
    .await?;
    println!("store: {}", store.root().display());

    let (adapter, adapter_events) = EmberAdapter::serial(&path).build();

    println!("\n=== starting the runtime ===");
    let zigbee = match Zigbee::builder(adapter, adapter_events, store)
        .network(NetworkConfig {
            pan_id: None,
            extended_pan_id: None,
            channel: 11,
            network_key: None,
            on_mismatch: if may_form {
                MismatchPolicy::Form
            } else {
                MismatchPolicy::Fail
            },
        })
        // Nothing has joined, so there is nothing to interview automatically.
        // The coordinator is interviewed explicitly below.
        .interview_on_join(false)
        .start()
        .await
    {
        Ok(zigbee) => zigbee,
        Err(e) => {
            println!("start refused: {e}");
            if !may_form {
                println!("\nPass --form to create a new network. That writes a fresh");
                println!("network key to the dongle and orphans anything already joined.");
            }
            return Ok(());
        }
    };

    println!("coordinator:   {}", zigbee.coordinator());
    println!("start outcome: {:?}", zigbee.start_outcome());
    match zigbee.network().await {
        Ok(n) => println!(
            "network:       pan 0x{:04x}, channel {}, nwk_update_id {}",
            n.pan_id, n.channel, n.nwk_update_id
        ),
        Err(e) => println!("network:       unavailable ({e})"),
    }

    // The device table should contain the coordinator even on a fresh network:
    // it is a node like any other, and without a record for it nothing can
    // address it.
    println!("\n=== device table ===");
    let devices = zigbee.devices().await?;
    for device in &devices {
        println!(
            "  {} nwk 0x{:04x} {:?} {:?}",
            device.ieee,
            device.nwk.raw(),
            device.kind,
            device.power_source
        );
    }
    let coordinator = zigbee.coordinator();
    if devices.iter().any(|d| d.ieee == coordinator) {
        println!("  -> the coordinator has a record, so it can be addressed");
    } else {
        println!("  !! the coordinator has NO record; requests to it will be refused");
    }

    // Joining first when it is wanted. The coordinator self-tests below take
    // about twelve seconds, and a device's pairing window is finite — spending
    // that time before opening the window cost one attempt already.
    if permit_join {
        println!("\n=== opening joining for 240s ===");
        zigbee.permit_join(Duration::from_secs(240), None).await?;
        println!("put the device in pairing mode now; events follow");
        drain_events(&zigbee, 240).await;
    } else {
        read_own_basic(&zigbee).await;
        interview_coordinator(&zigbee).await;
        drain_events(&zigbee, 3).await;
    }

    println!("\n=== final device table ===");
    for device in zigbee.devices().await? {
        println!(
            "  {} nwk 0x{:04x} {:?} interview {:?} model {:?}",
            device.ieee,
            device.nwk.raw(),
            device.kind,
            device.interview,
            device.basic.model_id
        );
    }

    zigbee.stop().await?;
    println!("\nstopped cleanly");
    Ok(())
}

/// Reads the coordinator's own `genBasic`.
///
/// The part that has only ever run against the mock: the request goes out, the
/// response arrives asynchronously as an adapter event, and the runtime matches
/// it to this caller by transaction sequence.
async fn read_own_basic(zigbee: &Zigbee) {
    let coordinator = zigbee.coordinator();
    println!("\n=== reading the coordinator's own genBasic ===");
    println!("(this exercises ZCL transaction correlation: the response arrives");
    println!(" asynchronously as an adapter event and is matched by sequence)");
    let attributes = vec![
        AttrId(0x0000), // zclVersion
        AttrId(0x0001), // appVersion
        AttrId(0x0003), // hwVersion
        AttrId(0x0004), // manufacturerName
        AttrId(0x0005), // modelId
        AttrId(0x0007), // powerSource
    ];
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(8),
        zigbee.zcl_read(coordinator, EndpointId(1), GEN_BASIC, attributes),
    )
    .await;
    println!("  elapsed: {:?}", started.elapsed());
    match outcome {
        Ok(Ok(values)) if values.is_empty() => {
            println!("  answered, but with no attribute records");
        }
        Ok(Ok(values)) => {
            for (id, value) in values {
                println!("  0x{id:04x} = {value:?}");
            }
        }
        // Expected, and worth saying so: EmberZNet does not answer a ZCL
        // unicast addressed to its own node id. It loops the request back to
        // the local application instead, which is how a correlation bug was
        // found here — the echo used to resolve the read with nothing in 27ms.
        // A timeout is now the correct outcome.
        Ok(Err(e)) => println!("  read failed (expected against self): {e}"),
        Err(_) => println!("  no response within 8s"),
    }
}

/// Interviews the coordinator through the runtime.
async fn interview_coordinator(zigbee: &Zigbee) {
    let coordinator = zigbee.coordinator();
    println!("\n=== interviewing the coordinator through the runtime ===");
    match tokio::time::timeout(Duration::from_secs(30), zigbee.interview(coordinator)).await {
        Ok(Ok(outcome)) => {
            println!("  state:     {:?}", outcome.state);
            println!("  completed: {:?}", outcome.completed);
            for (step, why) in &outcome.failures {
                println!("  failed:    {step:?}: {why}");
            }
            for endpoint in &outcome.endpoints {
                println!(
                    "  endpoint {} profile {:?} device 0x{:04x}, in {}, out {}",
                    endpoint.id.0,
                    endpoint.profile,
                    endpoint.device_id,
                    endpoint.input_clusters.len(),
                    endpoint.output_clusters.len()
                );
            }
            if let Some(basic) = &outcome.basic {
                println!("  basic:     {basic:?}");
            }
        }
        Ok(Err(e)) => println!("  interview failed: {e}"),
        Err(_) => println!("  interview did not finish within 30s"),
    }
}

/// Drains whatever the runtime reports.
///
/// With `--permit-join` this is where a real device would show up; without it,
/// it confirms the event stream is live and that nothing unexpected arrives.
async fn drain_events(zigbee: &Zigbee, window: u64) {
    use std::io::Write as _;

    println!("\n=== events ===");
    // Flushed after every line. A join window is finite, and output that only
    // appears when the process exits cannot tell you whether the device is
    // being seen while there is still time to retry.
    let _ = std::io::stdout().flush();
    let mut events = zigbee.events();
    let deadline = tokio::time::sleep(Duration::from_secs(window));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = events.recv() => match event {
                Some(Event::ZclMessage(message)) => {
                    println!("  {} {:?}", message.ieee, message.kind);
                    let _ = std::io::stdout().flush();
                }
                Some(event) => {
                    println!("  {event:?}");
                    let _ = std::io::stdout().flush();
                }
                None => break,
            },
        }
    }
}
