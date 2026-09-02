//! Hardware spike: can the `ashv2` + `ezsp` crate stack drive real Silicon Labs
//! firmware, and if not, exactly which step fails?
//!
//! This exists to answer one question before `rszigbee-adapter-ember` is
//! written. The risk it retires: `ezsp` went from version 10 to 17 in six weeks
//! with two yanked releases, and the `uplg/maison` project needed a fork of
//! `ashv2` to handle the EZSP <= v13 legacy `importTransientKey` wire format on
//! a common MG21 dongle. Finding that out here costs an afternoon. Finding it
//! out halfway through an adapter costs a rewrite.
//!
//! # This probe is strictly read-only
//!
//! It sends `nop`, `getEui64`, `networkState`, `getNetworkParameters`,
//! `getValue` and `getConfigurationValue` — nothing else. It does **not** form
//! or leave a network, does not open permit-join, does not write configuration,
//! and does not touch NVM3 tokens. Running it against a dongle that is currently
//! serving a live Zigbee network is safe.
//!
//! That constraint is deliberate and load-bearing: the destructive EZSP calls
//! (`formNetwork`, `leaveNetwork`, `setInitialSecurityState`) are not imported,
//! so it is a compile error for this file to acquire one by accident.
//!
//! # Usage
//!
//! ```text
//! cargo run -- /dev/cu.usbserial-XXXX
//! cargo run -- /dev/cu.usbmodem1101 --baud 230400 --no-rtscts --ezsp-version 8
//! RUST_LOG=trace cargo run -- /dev/cu.usbserial-XXXX   # full ASH frame trace
//! ```

use std::num::NonZero;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ezsp::{Client, Configuration, Networking, Utilities, ezsp::value};
use tokio_serial::{FlowControl, SerialPortBuilderExt};

/// Per-command deadline. Generous: a dongle that has just been plugged in can
/// take a while to answer the first frame, and a false "timeout" here would send
/// us hunting the wrong problem.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Channel depth for both the ASH payload channel and the EZSP actor channels.
const CHANNEL_SIZE: usize = 32;

/// EZSP protocol versions worth trying, newest first.
///
/// 13 covers current EmberZNet 7.x firmware; 8 covers older 6.x builds. The
/// probe walks the list because a version mismatch is the single most likely
/// failure and the error it produces is otherwise indistinguishable from a
/// wiring problem.
const VERSIONS_TO_TRY: &[u8] = &[13, 12, 9, 8];

struct Args {
    path: String,
    baud: u32,
    rtscts: bool,
    version: Option<u8>,
}

fn parse_args() -> Result<Args> {
    let mut path = None;
    let mut baud = 115_200;
    let mut rtscts = true;
    let mut version = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--baud" => {
                baud = it
                    .next()
                    .context("--baud needs a value")?
                    .parse()
                    .context("--baud must be a number")?;
            }
            "--no-rtscts" => rtscts = false,
            "--ezsp-version" => {
                version = Some(
                    it.next()
                        .context("--ezsp-version needs a value")?
                        .parse()
                        .context("--ezsp-version must be a number")?,
                );
            }
            "-h" | "--help" => {
                println!(
                    "usage: ezsp-probe <serial-path> [--baud N] [--no-rtscts] [--ezsp-version N]"
                );
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown flag: {other}"),
            other => path = Some(other.to_owned()),
        }
    }

    Ok(Args {
        path: path.context(
            "no serial path given.\n\
             On macOS look for /dev/cu.usbserial-* or /dev/cu.usbmodem*;\n\
             on Linux /dev/ttyUSB* or /dev/ttyACM*.",
        )?,
        baud,
        rtscts,
        version,
    })
}

/// One probe step and what it told us.
struct Step {
    name: &'static str,
    outcome: Result<String>,
}

fn report(steps: &[Step]) -> bool {
    println!("\n─── probe report ───");
    let mut all_ok = true;
    for s in steps {
        match &s.outcome {
            Ok(detail) => println!("  ok    {:<24} {detail}", s.name),
            Err(e) => {
                all_ok = false;
                println!("  FAIL  {:<24} {e:#}", s.name);
            }
        }
    }
    println!("────────────────────");
    all_ok
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args()?;

    println!("ezsp-probe — READ-ONLY. No network form, no permit-join, no writes.");
    println!(
        "port {} @ {} baud, rtscts {}",
        args.path,
        args.baud,
        if args.rtscts { "on" } else { "off" }
    );

    // Pre-flight the port. A path that cannot be opened is fatal and unrelated
    // to which EZSP version the firmware speaks; retrying the version list
    // against it would print the same error once per candidate.
    match open_port(&args) {
        Ok(p) => {
            drop(p);
            println!("  ok    serial open");
        }
        Err(e) => {
            println!("  FAIL  serial open           {e:#}");
            bail!("cannot open {}", args.path);
        }
    }

    // Each attempt then rebuilds the whole stack from a fresh port: ASH session
    // state does not survive a failed EZSP negotiation, so retrying on the same
    // transport would probe a desynchronised link.
    let candidates: Vec<u8> = args
        .version
        .map_or_else(|| VERSIONS_TO_TRY.to_vec(), |v| vec![v]);
    let mut last_error = None;
    let mut session = None;

    for want in candidates.iter().copied() {
        match connect_once(&args, want).await {
            Ok(conn) => {
                println!("  ok    EZSP negotiated       version {want}");
                session = Some((want, conn));
                break;
            }
            Err(e) => {
                println!("  ..    EZSP v{want:<16} {e:#}");
                last_error = Some(e);
            }
        }
    }

    let Some((negotiated, mut conn)) = session else {
        let e = last_error.unwrap_or_else(|| anyhow::anyhow!("no versions attempted"));
        println!("\n  FAIL  EZSP negotiation      {e:#}");
        println!(
            "\n        No EZSP version was accepted. In order of likelihood:\n\
             \x20         - wrong baud rate (try --baud 230400)\n\
             \x20         - hardware flow control mismatch (try --no-rtscts)\n\
             \x20         - the dongle runs Z-Stack or RCP firmware, not EmberZNet\n\
             \x20         - the dongle is sitting in its bootloader\n\
             \x20         - another process holds the port\n\
             \x20       RUST_LOG=trace shows whether any ASH bytes arrived at all,\n\
             \x20       which separates a wiring problem from a protocol one."
        );
        bail!("EZSP negotiation failed for every candidate version");
    };

    let mut steps = Vec::new();

    // ---- step 4: read-only queries ----------------------------------------
    macro_rules! probe {
        ($name:literal, $call:expr, $fmt:expr) => {{
            let outcome = match tokio::time::timeout(COMMAND_TIMEOUT, $call).await {
                Ok(Ok(v)) => Ok(($fmt)(v)),
                Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
                Err(_) => Err(anyhow::anyhow!("timed out after {COMMAND_TIMEOUT:?}")),
            };
            steps.push(Step {
                name: $name,
                outcome,
            });
        }};
    }

    probe!("nop", conn.nop(), |()| "round trip works".to_owned());

    probe!("getEui64", conn.get_eui64(), |eui| format!(
        "coordinator {eui}"
    ));

    probe!("networkState", conn.network_state(), |s| format!("{s:?}"));

    probe!(
        "getNetworkParameters",
        conn.get_network_parameters(),
        |(node_type, params): (ezsp::ember::node::Type, ezsp::ember::network::Parameters)| format!(
            "{node_type:?} · pan 0x{:04x} · ext_pan {} · channel {} · nwk_update_id {}",
            params.pan_id(),
            params.extended_pan_id(),
            params.radio_channel(),
            params.nwk_update_id(),
        )
    );

    // The three values every adapter reads at bring-up. If these work, the
    // crate stack is good enough to build the adapter on.
    probe!(
        "getValue(VERSION_INFO)",
        conn.get_value(value::Id::VersionInfo),
        |v| format!("{v:02x?}")
    );

    probe!(
        "getConfig(APS_UNICAST_MESSAGE_COUNT)",
        conn.get_configuration_value(ezsp::ezsp::config::Id::ApsUnicastMessageCount),
        |v| format!("{v}")
    );

    let all_ok = report(&steps);

    println!();
    if all_ok {
        println!("VERDICT: the ashv2 + ezsp stack drives this firmware.");
        println!(
            "         EZSP v{}. Proceed with rszigbee-adapter-ember and pin\n\
             \x20        ashv2 =13.0.0 / ezsp =17.0.0.",
            negotiated
        );
    } else {
        println!("VERDICT: partial. Negotiation worked, some commands did not.");
        println!(
            "         Note which. A failure in getNetworkParameters or getValue\n\
             \x20        means an encoding gap in the crate, and that is the thing\n\
             \x20        to reproduce as a minimal test and take upstream."
        );
    }

    Ok(())
}

/// Opens the port, brings up ASHv2 and EZSP, and negotiates one version.
///
/// Returns a live connection or the reason it failed. Everything is torn down on
/// failure: dropping the ASH handle closes the outbound queue, which terminates
/// the transmitter and then the receiver.
async fn connect_once(args: &Args, want: u8) -> Result<ezsp::Connection> {
    let (reader, writer) = tokio::io::split(open_port(args)?);

    // ashv2 owns the reset handshake, byte stuffing, CRC, ACK/NAK and
    // retransmission. If this layer is wrong, everything above it lies.
    let (payload_tx, payload_rx) = tokio::sync::mpsc::channel(CHANNEL_SIZE);
    let (ash_handle, ash_futures) = ashv2::start(reader, writer, payload_tx);
    tokio::spawn(ash_futures.transmitter);
    tokio::spawn(ash_futures.receiver);

    let ezsp_rx = ashv2::ezsp::Receiver::new(payload_rx);
    let (client, ezsp_futures) = Client::run(ash_handle, ezsp_rx, CHANNEL_SIZE);
    tokio::spawn(ezsp_futures.transmitter);
    tokio::spawn(ezsp_futures.receiver);

    let version = NonZero::new(want).context("EZSP version must be non-zero")?;
    match tokio::time::timeout(COMMAND_TIMEOUT, client.connect(version)).await {
        Ok(Ok((conn, _callbacks))) => Ok(conn),
        Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
        Err(_) => Err(anyhow::anyhow!("no response within {COMMAND_TIMEOUT:?}")),
    }
}

/// Opens the serial port with the requested settings.
fn open_port(args: &Args) -> Result<tokio_serial::SerialStream> {
    tokio_serial::new(&args.path, args.baud)
        .flow_control(if args.rtscts {
            FlowControl::Hardware
        } else {
            FlowControl::None
        })
        .timeout(Duration::from_secs(1))
        .open_native_async()
        .with_context(|| {
            format!(
                "could not open {}. Check the path exists, and that nothing else \
                 (Zigbee2MQTT, ZHA, another probe) holds the port.",
                args.path
            )
        })
}
