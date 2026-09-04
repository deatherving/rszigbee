//! The runtime against real hardware.
//!
//! ```text
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --form
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --permit-join
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --configure
//! cargo run -p rszigbee --example ember_runtime -- /dev/ttyUSB0 --actuate
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
//! # What needed a second device, and has now had one
//!
//! Binding, attribute reporting, inbound reports and commands cannot be
//! exercised against the coordinator alone. All four are now confirmed against
//! a SONOFF SWV-ZNU water valve:
//!
//! * `--permit-join` — joins, commissions, interviews, and resolves a
//!   definition from the bundled set.
//! * `--configure` — binds and configures reporting. Two bindings, two
//!   reporting configurations, no failures.
//! * `--actuate` — `SetOn(true)` and `SetOn(false)`, each answered by the
//!   device reporting its own new state back as typed `StateChanged`.
//!
//! `--actuate` moves something physical. The device this was written for is a
//! water valve, which is why it is a flag and not part of the default path.
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
use rszigbee::{DeviceCommand, Event, FileStore, Zigbee};

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
        .ok_or(
            "usage: ember_runtime <serial-path> [--form] [--permit-join] [--actuate] [--configure]",
        )?
        .clone();
    let may_form = args.iter().any(|a| a == "--form");
    let permit_join = args.iter().any(|a| a == "--permit-join");
    let actuate = args.iter().any(|a| a == "--actuate");
    let configure = args.iter().any(|a| a == "--configure");

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
        // With --permit-join a real device can arrive, and interviewing it is
        // the point: the interview is what resolves a definition, and until a
        // device joined on hardware that path had only ever run against the
        // mock. Without the flag nothing can join, so the coordinator is
        // interviewed explicitly below instead.
        .interview_on_join(permit_join)
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

    report_startup(&zigbee).await?;

    // Joining first when it is wanted. The coordinator self-tests below take
    // about twelve seconds, and a device's pairing window is finite — spending
    // that time before opening the window cost one attempt already.
    if configure {
        configure_a_device(&zigbee).await;
    } else if actuate {
        actuate_a_device(&zigbee).await;
    } else if permit_join {
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

/// Prints what the coordinator came up as, and what it knows about.
///
/// Split out of `main` because it is the same preamble whichever mode runs, and
/// because `main` had grown past the point where the mode dispatch was visible
/// in it.
async fn report_startup(zigbee: &Zigbee) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}

/// Binds and configures reporting on a real device, printing every step.
///
/// The step the runtime already runs after an interview, made visible. It
/// matters more than it looks: bind without configure and many devices report
/// only when polled, so a sensor that appears to work goes quiet as soon as
/// nothing is asking. Whether a *particular* device accepted the configuration
/// is not something the mock can answer.
async fn configure_a_device(zigbee: &Zigbee) {
    let coordinator = zigbee.coordinator();
    let devices = match zigbee.devices().await {
        Ok(devices) => devices,
        Err(e) => {
            println!("cannot list devices: {e}");
            return;
        }
    };
    let Some(target) = devices.iter().find(|d| d.ieee != coordinator) else {
        println!("\n=== no device to configure ===");
        println!("Only the coordinator is known. Pair a device first with --permit-join.");
        return;
    };

    println!("\n=== configure plan for {} ===", target.ieee);
    match zigbee.configure_plan(target.ieee).await {
        Ok(steps) if steps.is_empty() => {
            println!("  (empty: the definition asks for no bindings or reporting)");
        }
        Ok(steps) => {
            for step in &steps {
                match (step.attribute, step.attribute_type) {
                    (Some(attribute), ty) => println!(
                        "  ep {} cluster 0x{:04x} attr 0x{:04x} {:?} every {}..{}s",
                        step.endpoint.0,
                        step.cluster.0,
                        attribute.0,
                        ty,
                        step.min_interval,
                        step.max_interval
                    ),
                    (None, _) => println!(
                        "  ep {} cluster 0x{:04x} bind only",
                        step.endpoint.0, step.cluster.0
                    ),
                }
            }
        }
        Err(e) => println!("  cannot plan: {e}"),
    }

    println!("\n=== executing it ===");
    // Longer than the ZDO and ZCL timeouts combined: every step is a round trip
    // to a sleepy device that has to poll before it hears anything.
    match tokio::time::timeout(Duration::from_secs(90), zigbee.configure(target.ieee)).await {
        Ok(Ok(outcome)) => {
            println!(
                "  bound {}, configured {}, failed {}",
                outcome.bound, outcome.configured, outcome.failed
            );
            if outcome.failed > 0 {
                println!("  -> a failure here is the device refusing, not a transport error;");
                println!(
                    "     many devices accept the bind and refuse reporting on some attributes"
                );
            }
        }
        Ok(Err(e)) => println!("  refused: {e}"),
        Err(_) => println!("  did not finish within 90s"),
    }
}

/// Sends a real command to a real device, and watches for the result.
///
/// The last path in the runtime that had only ever run against the mock. The
/// actuator direction is not symmetric with the sensor one: a command has to
/// resolve a capability to a cluster and endpoint from the definition, encode a
/// ZCL command rather than decode one, and reach a device that may be asleep.
///
/// Deliberately a separate flag. This turns a physical thing on -- the device
/// this was written for is a water valve -- so it does not belong on a path
/// anyone might run to look at a device table.
async fn actuate_a_device(zigbee: &Zigbee) {
    use std::io::Write as _;

    let coordinator = zigbee.coordinator();
    let devices = match zigbee.devices().await {
        Ok(devices) => devices,
        Err(e) => {
            println!("cannot list devices: {e}");
            return;
        }
    };
    let Some(target) = devices.iter().find(|d| d.ieee != coordinator) else {
        println!("\n=== no device to actuate ===");
        println!("Only the coordinator is known. Pair a device first with --permit-join.");
        return;
    };

    println!("\n=== actuating {} ===", target.ieee);
    println!(
        "model {:?}, interview {:?}",
        target.basic.model_id, target.interview
    );

    // Events first, so a report caused by the command is not missed between
    // the send returning and the stream being subscribed.
    let mut events = zigbee.events();

    for on in [true, false] {
        println!("\n-> SetOn({on})");
        let _ = std::io::stdout().flush();
        let started = std::time::Instant::now();
        // Generous: the target is a sleepy end device, so the coordinator holds
        // the frame until the device next polls its parent. A timeout here is
        // not the same as a refusal, and the two are reported differently.
        match tokio::time::timeout(
            Duration::from_secs(20),
            zigbee.send(target.ieee, DeviceCommand::SetOn(on)),
        )
        .await
        {
            Ok(Ok(outcome)) => println!("   accepted in {:?}: {outcome:?}", started.elapsed()),
            Ok(Err(e)) => println!("   refused: {e}"),
            Err(_) => println!("   no answer within 20s (a sleepy device may not have polled)"),
        }
        let _ = std::io::stdout().flush();

        // Then watch for the device to say so itself. A command the coordinator
        // accepted is not yet a valve that moved.
        println!("   waiting for the device to report its new state...");
        let _ = std::io::stdout().flush();
        let deadline = tokio::time::sleep(Duration::from_secs(15));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => {
                    println!("   (no report within 15s)");
                    break;
                }
                event = events.recv() => match event {
                    Some(Event::StateChanged { ieee, changes, .. }) if ieee == target.ieee => {
                        println!("   StateChanged: {changes:?}");
                        let _ = std::io::stdout().flush();
                        break;
                    }
                    Some(Event::ZclMessage(message)) if message.ieee == target.ieee => {
                        println!("   raw: {:?}", message.kind);
                        let _ = std::io::stdout().flush();
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
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
