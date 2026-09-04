//! The MQTT gateway against real hardware and a real broker.
//!
//! ```text
//! cargo run -p rszigbee --features gateway --example mqtt_gateway -- \
//!   /dev/ttyUSB0 --broker 127.0.0.1
//! ```
//!
//! What it proves that a unit test cannot: that the topics are the ones a
//! broker actually delivers, that a `/set` published by an unrelated client
//! reaches a device, and that the will is registered before connecting rather
//! than after — which looks like ordinary configuration and silently does
//! nothing if done in the wrong order.
//!
//! Compare against a reference gateway by pointing both at the same broker in
//! turn and diffing what a subscriber sees:
//!
//! ```text
//! mosquitto_sub -h 127.0.0.1 -t 'zigbee2mqtt/#' -v
//! mosquitto_pub -h 127.0.0.1 -t 'zigbee2mqtt/<ieee>/set' -m '{"state":"ON"}'
//! ```
//!
//! `--permit-join` opens joining for four minutes at startup. Without it the
//! gateway runs against whatever is already paired.

use std::time::Duration;

use rszigbee::adapter::{MismatchPolicy, NetworkConfig};
use rszigbee::ember::EmberAdapter;
use rszigbee::gateway::{GatewayConfig, run};
use rszigbee::mqtt::Topics;
use rszigbee::{FileStore, Zigbee};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: mqtt_gateway <serial-path> [--broker HOST] [--permit-join]")?
        .clone();
    let broker = flag(&args, "--broker").unwrap_or_else(|| "127.0.0.1".to_owned());
    let base = flag(&args, "--base-topic").unwrap_or_else(|| "zigbee2mqtt".to_owned());
    let permit_join = args.iter().any(|a| a == "--permit-join");

    let store = FileStore::open(
        std::env::var("RSZIGBEE_DATA").unwrap_or_else(|_| "./rszigbee-data".into()),
    )
    .await?;
    let (adapter, adapter_events) = EmberAdapter::serial(&path).build();

    let zigbee = Zigbee::builder(adapter, adapter_events, store)
        .network(NetworkConfig {
            pan_id: None,
            extended_pan_id: None,
            channel: 11,
            network_key: None,
            // Never form from the gateway. Forming here would orphan every
            // device the user owns, and a gateway is exactly the context where
            // that would be noticed last.
            on_mismatch: MismatchPolicy::Fail,
        })
        .start()
        .await?;

    println!("coordinator: {}", zigbee.coordinator());
    for device in zigbee.devices().await? {
        println!(
            "  {} {:?} model {:?}",
            device.ieee, device.kind, device.basic.model_id
        );
    }

    if permit_join {
        zigbee.permit_join(Duration::from_secs(240), None).await?;
        println!("joining open for 240s");
    }

    println!("\nconnecting to mqtt://{broker}:1883 under {base}/");
    println!("subscribe with: mosquitto_sub -h {broker} -t '{base}/#' -v\n");

    let outcome = run(
        &zigbee,
        GatewayConfig {
            host: broker,
            topics: Topics::new(&base),
            ..GatewayConfig::default()
        },
    )
    .await;

    // Reported rather than swallowed: the gateway returning at all is
    // information, and "the runtime stopped" and "the broker went away for
    // good" are different problems.
    println!("gateway stopped: {outcome:?}");
    zigbee.stop().await?;
    Ok(())
}

/// The value after `name`, if present.
fn flag(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).cloned()
}
