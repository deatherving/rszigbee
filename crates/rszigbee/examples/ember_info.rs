//! Bring an Ember coordinator up through the public API and print what it says.
//!
//! Read-only: it starts the adapter, reads identity and firmware, and drains
//! events. It does not form a network, open permit-join, or write anything.
//!
//!   cargo run -p rszigbee --example ember_info -- /dev/ttyUSB0
use std::time::Duration;

use rszigbee::adapter::{CoordinatorAdapter, MismatchPolicy, NetworkConfig};
use rszigbee::ember::EmberAdapter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let path = std::env::args()
        .nth(1)
        .ok_or("usage: ember_info <serial-path>")?;
    if let Some(d) = rszigbee::ember::recognise(&path) {
        println!("recognised: {} ({:?})", d.name, d.settings);
    } else {
        println!("unrecognised dongle; using fallback serial settings");
    }

    let (mut adapter, mut events) = EmberAdapter::serial(&path).build();

    let config = NetworkConfig {
        pan_id: None,
        extended_pan_id: None,
        channel: 11,
        network_key: None,
        // Never form a network we did not mean to form.
        on_mismatch: MismatchPolicy::Fail,
    };

    match adapter.start(&config, None).await {
        Ok(outcome) => println!("start: {outcome:?}"),
        Err(e) => {
            println!("start refused: {e}");
            println!("\nThat refusal is the safety behaviour working, not a bug:");
            println!("a coordinator with no network formed cannot be resumed, and");
            println!("forming one would create a network no device is joined to.");
            return Ok(());
        }
    }

    println!("coordinator: {}", adapter.coordinator_ieee().await?);
    let fw = adapter.firmware().await?;
    println!("firmware:    {} ({})", fw.version, fw.family);
    for (k, v) in &fw.meta {
        println!("             {k} = {v}");
    }
    println!("capabilities: {:?}", adapter.capabilities());
    match adapter.network_info().await {
        Ok(n) => println!("network:     {n:?}"),
        Err(e) => println!("network:     unavailable ({e})"),
    }

    println!("\ndraining events for 5s...");
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            Some(event) = events.recv() => println!("  {event:?}"),
        }
    }

    adapter.stop().await?;
    println!("stopped cleanly");
    Ok(())
}
