//! Bringing an EZSP session up: port, `ASHv2`, version negotiation.
//!
//! Kept separate from the adapter because a failed negotiation cannot be
//! retried on the same transport — ASH session state does not survive it — so
//! "open and negotiate" is one indivisible operation that either yields a live
//! connection or nothing.

use std::time::Duration;

use ezsp::{Client, Connection};
use rszigbee_adapter::AdapterError;
use tokio_serial::{FlowControl, SerialPortBuilderExt, SerialStream};
use tracing::{debug, info, warn};

use crate::fingerprint::SerialSettings;

/// Channel depth for the ASH payload channel and the EZSP actor channels.
///
/// Bounded: an unbounded channel converts a slow consumer into unbounded memory
/// growth, which is the failure mode to avoid rather than the one to hide.
const CHANNEL_SIZE: usize = 64;

/// How long to wait for the NCP to answer the `version` command.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to allow the blocking `open(2)` before giving up on it.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// EZSP protocol versions to try, newest first.
///
/// 13 covers current `EmberZNet` 7.x, 8 covers older 6.x. Walking the list means
/// a user with older firmware does not have to know to configure it.
pub const VERSIONS: &[u8] = &[13, 12, 9, 8];

/// A live EZSP session.
pub struct Session {
    /// The negotiated protocol version.
    pub version: u8,
    /// The cloneable command interface.
    pub connection: Connection,
    /// Asynchronous callbacks from the NCP.
    pub callbacks: tokio::sync::mpsc::Receiver<ezsp::Callback>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Opens the serial port, with a deadline.
///
/// `open(2)` on a tty with hardware flow control blocks in the kernel until CTS
/// is asserted. That is not an await point, so `tokio::time::timeout` around an
/// async wrapper does nothing — observed as a ten-minute hang with a
/// five-second timeout configured. The open therefore runs on a blocking thread
/// with its own deadline; if it expires the thread is abandoned rather than
/// leaked into the request path.
async fn open_port(path: &str, settings: SerialSettings) -> Result<SerialStream, AdapterError> {
    let owned = path.to_owned();
    let handle = tokio::task::spawn_blocking(move || {
        tokio_serial::new(&owned, settings.baud)
            .flow_control(if settings.rtscts {
                FlowControl::Hardware
            } else {
                FlowControl::None
            })
            .timeout(Duration::from_millis(500))
            .open_native_async()
            .map_err(|e| e.to_string())
    });

    match tokio::time::timeout(OPEN_TIMEOUT, handle).await {
        Ok(Ok(Ok(port))) => Ok(port),
        Ok(Ok(Err(e))) => Err(AdapterError::Transport(format!("cannot open {path}: {e}"))),
        Ok(Err(e)) => Err(AdapterError::Transport(format!("open task failed: {e}"))),
        Err(_) => Err(AdapterError::Transport(format!(
            "opening {path} did not complete within {OPEN_TIMEOUT:?}. \
             This is what a hardware flow control mismatch looks like: open(2) \
             blocks in the kernel waiting for CTS. Try flow control off."
        ))),
    }
}

/// Opens the port and negotiates one specific EZSP version.
async fn negotiate(
    path: &str,
    settings: SerialSettings,
    want: u8,
) -> Result<Session, AdapterError> {
    let port = open_port(path, settings).await?;
    let (reader, writer) = tokio::io::split(port);

    // ashv2 owns the reset handshake, byte stuffing, CRC, ACK/NAK and
    // retransmission. Dropping the handle closes the outbound queue, which
    // terminates the transmitter and then the receiver, so a failed attempt
    // does not leak tasks.
    let (payload_tx, payload_rx) = tokio::sync::mpsc::channel(CHANNEL_SIZE);
    let (ash_handle, ash_futures) = ashv2::start(reader, writer, payload_tx);
    tokio::spawn(ash_futures.transmitter);
    tokio::spawn(ash_futures.receiver);

    let ezsp_rx = ashv2::ezsp::Receiver::new(payload_rx);
    let (client, ezsp_futures) = Client::run(ash_handle, ezsp_rx, CHANNEL_SIZE);
    tokio::spawn(ezsp_futures.transmitter);
    tokio::spawn(ezsp_futures.receiver);

    let version = std::num::NonZero::new(want)
        .ok_or_else(|| AdapterError::Transport("EZSP version must be non-zero".into()))?;

    match tokio::time::timeout(NEGOTIATE_TIMEOUT, client.connect(version)).await {
        Ok(Ok((connection, callbacks))) => Ok(Session {
            version: want,
            connection,
            callbacks,
        }),
        Ok(Err(e)) => Err(AdapterError::IncompatibleFirmware(format!(
            "NCP rejected EZSP version {want}: {e}"
        ))),
        Err(_) => Err(AdapterError::Timeout(NEGOTIATE_TIMEOUT)),
    }
}

/// Opens the port and negotiates, trying `candidates` in order.
///
/// Each attempt rebuilds the whole stack from a fresh port, because ASH session
/// state does not survive a failed negotiation and retrying on the same
/// transport would probe a desynchronised link.
pub async fn connect(
    path: &str,
    settings: SerialSettings,
    candidates: &[u8],
) -> Result<Session, AdapterError> {
    let mut last = None;

    for want in candidates.iter().copied() {
        debug!(version = want, path, "negotiating EZSP");
        match negotiate(path, settings, want).await {
            Ok(session) => {
                info!(version = want, path, "EZSP session established");
                return Ok(session);
            }
            Err(e) => {
                // A transport failure is fatal and has nothing to do with the
                // version, so stop rather than repeating it once per candidate.
                if matches!(e, AdapterError::Transport(_)) {
                    return Err(e);
                }
                warn!(version = want, error = %e, "EZSP version not accepted");
                last = Some(e);
            }
        }
    }

    Err(last.unwrap_or_else(|| {
        AdapterError::IncompatibleFirmware("no EZSP versions were attempted".into())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_list_is_newest_first_and_plausible() {
        // Order matters: negotiating an old version against new firmware
        // succeeds but loses commands, so the newest must be tried first.
        assert_eq!(VERSIONS.first(), Some(&13));
        let mut sorted = VERSIONS.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(VERSIONS, sorted.as_slice(), "candidates must be descending");
        assert!(VERSIONS.iter().all(|v| *v >= 4 && *v <= 20));
    }

    #[tokio::test]
    async fn a_nonexistent_port_fails_fast_as_a_transport_error() {
        // Must be Transport, not IncompatibleFirmware: `connect` uses that
        // distinction to stop instead of retrying every version against a path
        // that will never open.
        let e = connect(
            "/dev/definitely-not-a-real-port",
            SerialSettings::FALLBACK,
            VERSIONS,
        )
        .await
        .expect_err("must fail");
        assert!(matches!(e, AdapterError::Transport(_)), "got {e:?}");
        assert!(e.to_string().contains("definitely-not-a-real-port"));
    }

    #[tokio::test]
    async fn a_transport_failure_is_not_retried_per_version() {
        // Regression guard: the first version of this logic printed the same
        // "cannot open" error four times.
        let start = std::time::Instant::now();
        let _ = connect(
            "/dev/definitely-not-a-real-port",
            SerialSettings::FALLBACK,
            VERSIONS,
        )
        .await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should have given up immediately, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn an_empty_candidate_list_is_an_error_not_a_hang() {
        let e = connect("/dev/null", SerialSettings::FALLBACK, &[])
            .await
            .expect_err("must fail");
        assert!(
            matches!(e, AdapterError::IncompatibleFirmware(_)),
            "got {e:?}"
        );
    }
}
