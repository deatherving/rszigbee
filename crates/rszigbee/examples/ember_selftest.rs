//! Form a network and interview the coordinator through the ZDO path.
//!
//! The coordinator is itself a Zigbee node at `nwk 0x0000`, so this exercises
//! the whole `send_zdo` path — request encoding, sequence correlation, the
//! response callback, the ZDO decoders — with **no end device required**.
//! zigbee-herdsman does the same thing at startup to build the coordinator's
//! own device record.
//!
//! ```text
//! cargo run -p rszigbee --example ember_selftest -- /dev/ttyUSB0
//! cargo run -p rszigbee --example ember_selftest -- /dev/ttyUSB0 --form
//! ```
//!
//! # `--form` writes to the dongle
//!
//! Without it, a coordinator that has no network makes this exit early rather
//! than forming one. With it, a **new network is created**: a fresh network key
//! is generated and written, and any device joined to a previous network on
//! this coordinator is orphaned. Safe on a blank dongle, destructive otherwise.

use std::time::Duration;

use rszigbee::adapter::{
    AdapterEvent, CoordinatorAdapter, Destination, MismatchPolicy, NetworkConfig, TxOptions, ZdoTx,
};
use rszigbee::ember::EmberAdapter;
use rszigbee::spec::zdo::{
    self, ZdoClusterId, decode_active_ep_rsp, decode_node_desc_rsp, decode_simple_desc_rsp,
};
use rszigbee::{FileStore, Ieee, Nwk, PersistedNetwork, ZigbeeStore};

/// How long to wait for one ZDO response before giving up on it.
const ZDO_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: ember_selftest <serial-path> [--form]")?
        .clone();
    let may_form = args.iter().any(|a| a == "--form");

    // Persistence: without this, a formed network's key is lost when the
    // process exits, and every joined device would have to be re-paired.
    let store = FileStore::open(
        std::env::var("RSZIGBEE_DATA").unwrap_or_else(|_| "./rszigbee-data".into()),
    )
    .await?;
    println!("store: {}", store.root().display());
    let stored = store.load_network().await?;
    if let Some(n) = &stored {
        println!(
            "stored network: pan 0x{:04x}, channel {}, frame counter {}",
            n.pan_id, n.channel, n.frame_counter
        );
    } else {
        println!("stored network: none yet");
    }

    let (mut adapter, mut events) = EmberAdapter::serial(&path).build();

    let config = NetworkConfig {
        pan_id: None,
        extended_pan_id: None,
        channel: 11,
        network_key: None,
        on_mismatch: if may_form {
            MismatchPolicy::Form
        } else {
            MismatchPolicy::Fail
        },
    };

    println!("=== start ===");
    let outcome = match adapter.start(&config, None).await {
        Ok(o) => o,
        Err(e) => {
            println!("start refused: {e}");
            if !may_form {
                println!("\nPass --form to create a new network. That writes a fresh");
                println!("network key to the dongle and orphans anything already joined.");
            }
            return Ok(());
        }
    };
    println!("outcome: {outcome:?}");

    let coordinator = adapter.coordinator_ieee().await?;
    let live = adapter.network_info().await.ok();
    persist(&store, &mut adapter, stored.as_ref(), coordinator, live).await?;

    println!("coordinator: {coordinator}");
    println!("firmware:    {}", adapter.firmware().await?.version);
    match adapter.network_info().await {
        Ok(n) => println!(
            "network:     pan 0x{:04x}, channel {}, nwk_update_id {}",
            n.pan_id, n.channel, n.nwk_update_id
        ),
        Err(e) => println!("network:     unavailable ({e})"),
    }

    self_interview(&mut adapter, &mut events).await;

    adapter.stop().await?;
    println!("\nstopped cleanly");
    Ok(())
}

/// Writes what we learned about the network to the store.
///
/// A formed network must be persisted before anything else can fail: its key
/// exists only in memory until it is written, and a network whose key was never
/// written is lost on the next restart with no way to recover it.
async fn persist(
    store: &FileStore,
    // `&mut` because reading the network key is a coordinator round trip, and
    // storing a formed network without it is the one thing this function
    // exists to prevent.
    adapter: &mut EmberAdapter,
    stored: Option<&PersistedNetwork>,
    coordinator: Ieee,
    live: Option<rszigbee::adapter::NetworkInfo>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(formed) = adapter.formed_network() {
        println!(
            "formed: pan 0x{:04x}, ext_pan 0x{:016x}, channel {}",
            formed.pan_id, formed.extended_pan_id, formed.channel
        );
        // Persisted immediately, before anything else can fail. A formed
        // network whose key was never written is a network that is lost on the
        // next restart, and there is no way to recover it afterwards.
        store
            .save_network(&PersistedNetwork {
                pan_id: formed.pan_id,
                extended_pan_id: formed.extended_pan_id,
                channel: formed.channel,
                nwk_update_id: 0,
                coordinator_ieee: coordinator,
                key_sequence: 0,
                // A freshly formed network starts at zero. The runtime tracks
                // it from here, ahead of the live value, so a crash cannot
                // roll it back below what was transmitted.
                frame_counter: 0,
                // The key the adapter just generated, which it already
                // carries -- no need to ask the coordinator for it back. This
                // example used to store the network *without* a key and say
                // the coordinator holds it; a formed network stored that way
                // cannot be recreated on replacement hardware.
                network_key: Some(formed.network_key.clone()),
            })
            .await?;
        // The key itself is never printed.
        println!(
            "network key: generated and persisted ({} bytes)",
            formed.network_key.expose().len()
        );
    } else if let Some(n) = live {
        // Resumed rather than formed. The parameters are still worth writing:
        // they are how a later run notices that the coordinator was swapped or
        // that the channel moved. The network key is deliberately absent --
        // the coordinator holds it and will not hand it back, so a store record
        // after a resume can describe the network but not reconstruct it. That
        // is what coordinator backups are for.
        if let Some(prev) = &stored {
            if prev.coordinator_ieee != coordinator {
                // Exactly the situation that must never be resolved silently:
                // every device's link key was derived against the old address.
                println!(
                    "WARNING: stored coordinator {} but this one is {coordinator}",
                    prev.coordinator_ieee
                );
            } else if prev.pan_id != n.pan_id {
                println!(
                    "WARNING: stored pan 0x{:04x} but the coordinator is on 0x{:04x}",
                    prev.pan_id, n.pan_id
                );
            }
        }
        store
            .save_network(&PersistedNetwork {
                pan_id: n.pan_id,
                extended_pan_id: n.extended_pan_id,
                channel: n.channel,
                nwk_update_id: n.nwk_update_id,
                coordinator_ieee: coordinator,
                key_sequence: n.key_sequence,
                frame_counter: n.frame_counter,
                network_key: adapter.network_key().await.unwrap_or_default(),
            })
            .await?;
        println!("persisted the resumed network, key included where the coordinator exports it");
    }

    Ok(())
}

/// Interviews the coordinator over ZDO: node descriptor, endpoints, and a
/// simple descriptor for each endpoint.
///
/// This is the whole point of the example: it needs no other device on the
/// network, because the coordinator is itself a node at `nwk 0x0000`.
async fn self_interview(
    adapter: &mut EmberAdapter,
    events: &mut tokio::sync::mpsc::Receiver<AdapterEvent>,
) {
    println!("\n=== ZDO self-interview (target nwk 0x0000) ===");
    let mut seq: u8 = 0;
    let mut next_seq = move || {
        seq = seq.wrapping_add(1);
        seq
    };

    // Node descriptor: node type, manufacturer code, MAC capabilities.
    let s = next_seq();
    match zdo_roundtrip(
        adapter,
        events,
        ZdoClusterId::NODE_DESC_REQ,
        zdo::encode_node_desc_req(s, Nwk::COORDINATOR),
        s,
    )
    .await
    {
        Ok(payload) => match decode_node_desc_rsp(&payload) {
            Ok(d) => println!(
                "  Node_Desc_rsp     {:?}, manufacturer 0x{:04x}, rx_on_when_idle {}, mains {}",
                d.logical_type,
                d.manufacturer_code,
                d.rx_on_when_idle(),
                d.mains_powered()
            ),
            Err(e) => println!("  Node_Desc_rsp     decode failed: {e}"),
        },
        Err(e) => println!("  Node_Desc_req     {e}"),
    }

    // Active endpoints: which endpoints the coordinator registered.
    let s = next_seq();
    let endpoints = match zdo_roundtrip(
        adapter,
        events,
        ZdoClusterId::ACTIVE_EP_REQ,
        zdo::encode_active_ep_req(s, Nwk::COORDINATOR),
        s,
    )
    .await
    {
        Ok(payload) => match decode_active_ep_rsp(&payload) {
            Ok(a) => {
                println!("  Active_EP_rsp     endpoints {:?}", a.endpoints);
                a.endpoints
            }
            Err(e) => {
                println!("  Active_EP_rsp     decode failed: {e}");
                Vec::new()
            }
        },
        Err(e) => {
            println!("  Active_EP_req     {e}");
            Vec::new()
        }
    };

    // Simple descriptor for each endpoint: profile, device id, cluster lists.
    for ep in endpoints {
        let s = next_seq();
        match zdo_roundtrip(
            adapter,
            events,
            ZdoClusterId::SIMPLE_DESC_REQ,
            zdo::encode_simple_desc_req(s, Nwk::COORDINATOR, ep),
            s,
        )
        .await
        {
            Ok(payload) => match decode_simple_desc_rsp(&payload) {
                Ok(d) => println!(
                    "  Simple_Desc_rsp   ep{} profile {:?} device 0x{:04x}, in {:?}, out {:?}",
                    d.endpoint.0, d.profile, d.device_id, d.input_clusters, d.output_clusters
                ),
                Err(e) => println!("  Simple_Desc_rsp   ep{} decode failed: {e}", ep.0),
            },
            Err(e) => println!("  Simple_Desc_req   ep{} {e}", ep.0),
        }
    }
}

/// Sends a ZDO request and waits for the response carrying the same sequence.
///
/// The correlation lives here, in the caller, because that is where the ZDO
/// sequence number is allocated — the adapter cannot do it (see the
/// `rszigbee-adapter-ember` module docs).
async fn zdo_roundtrip(
    adapter: &mut EmberAdapter,
    events: &mut tokio::sync::mpsc::Receiver<AdapterEvent>,
    cluster: ZdoClusterId,
    payload: Vec<u8>,
    expect_sequence: u8,
) -> Result<Vec<u8>, String> {
    adapter
        .send_zdo(ZdoTx {
            dest: Destination::Unicast {
                ieee: Ieee::ZERO,
                nwk: Nwk::COORDINATOR,
            },
            cluster,
            payload,
            options: TxOptions::default(),
        })
        .await
        .map_err(|e| format!("send failed: {e}"))?;

    let deadline = tokio::time::sleep(ZDO_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => {
                return Err(format!("no response within {ZDO_TIMEOUT:?}"));
            }
            event = events.recv() => match event {
                Some(AdapterEvent::Zdo { cluster: got, payload, .. })
                    if got == cluster.response()
                        && payload.first() == Some(&expect_sequence) =>
                {
                    return Ok(payload);
                }
                // Anything else on the way through is noted, not silently
                // dropped: an unexpected frame during an interview is a clue.
                Some(other) => println!("    (while waiting: {other:?})"),
                None => return Err("event channel closed".into()),
            },
        }
    }
}
