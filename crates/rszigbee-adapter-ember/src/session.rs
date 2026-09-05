//! Bringing an EZSP session up: port, ASH, version negotiation.
//!
//! Kept separate from the adapter because a failed negotiation cannot be
//! retried on the same transport — ASH session state does not survive it — so
//! "open and negotiate" is one indivisible operation that either yields a live
//! connection or nothing.

use std::time::Duration;

use rsezsp::Ncp;
use rsezsp::transport::serial::{SerialSettings as RsSerialSettings, SerialTransport};
use rszigbee_adapter::AdapterError;
use tracing::{debug, info};

use crate::connection::Connection;
use crate::fingerprint::SerialSettings;

/// How long to allow the blocking `open(2)` before giving up on it.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to allow the reset handshake and version negotiation.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(10);

/// A live EZSP session.
#[derive(Debug)]
pub struct Session {
    /// The negotiated protocol version.
    pub version: u8,
    /// The cloneable command interface.
    pub connection: Connection,
    /// Asynchronous callbacks from the NCP.
    pub callbacks: tokio::sync::mpsc::Receiver<rsezsp::ezsp::callback::Callback>,
}

/// Opens the serial port, with a deadline.
///
/// `open(2)` on a tty with hardware flow control blocks in the kernel until CTS
/// is asserted. That is not an await point, so `tokio::time::timeout` around an
/// async wrapper does nothing — observed as a ten-minute hang with a
/// five-second timeout configured. The open therefore runs on a blocking thread
/// with its own deadline; if it expires the thread is abandoned rather than
/// leaked into the request path.
async fn open_port(path: &str, settings: SerialSettings) -> Result<SerialTransport, AdapterError> {
    let owned = path.to_owned();
    let handle = tokio::task::spawn_blocking(move || {
        SerialTransport::open(
            &owned,
            RsSerialSettings {
                baud: settings.baud,
                rtscts: settings.rtscts,
            },
        )
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

/// Opens the port and brings an EZSP session up.
///
/// # Version negotiation is not a search
///
/// This used to try a list of versions newest-first, rebuilding the whole stack
/// on each failure. That was working around a transport that treated the
/// version as something the host chose. It is not: the host offers a version
/// and the NCP answers with the one it runs, and `rsezsp` completes that
/// exchange itself — including the second round trip that a differing version
/// requires. One attempt is enough, and a failure now means something is
/// actually wrong rather than "try an older number".
///
/// # Errors
///
/// [`AdapterError::Transport`] if the port cannot be opened or the NCP does not
/// answer, and [`AdapterError::IncompatibleFirmware`] if it negotiates a
/// version this build does not know the wire format for.
pub async fn connect(path: &str, settings: SerialSettings) -> Result<Session, AdapterError> {
    let transport = open_port(path, settings).await?;
    debug!(path, "port open, negotiating EZSP");

    let ncp = match tokio::time::timeout(NEGOTIATE_TIMEOUT, Ncp::connect(transport)).await {
        Ok(Ok(ncp)) => ncp,
        Ok(Err(e)) => {
            return Err(match e {
                rsezsp::ezsp::EzspError::UnsupportedVersion { negotiated } => {
                    AdapterError::IncompatibleFirmware(format!(
                        "the NCP speaks EZSP {negotiated}, which this build does not support"
                    ))
                }
                other => AdapterError::Transport(format!("EZSP negotiation failed: {other}")),
            });
        }
        Err(_) => return Err(AdapterError::Timeout(NEGOTIATE_TIMEOUT)),
    };

    let version = ncp.version().raw();
    let stack_version = ncp.stack_version();
    let (connection, callbacks) = Connection::spawn(ncp);

    info!(
        version,
        stack_version = format_args!("{stack_version:#06x}"),
        path,
        "EZSP session established"
    );

    Ok(Session {
        version,
        connection,
        callbacks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_nonexistent_port_fails_fast_as_a_transport_error() {
        // Must be Transport rather than IncompatibleFirmware: they mean
        // different things to a caller deciding whether to try another device.
        let e = connect("/dev/definitely-not-a-real-port", SerialSettings::FALLBACK)
            .await
            .expect_err("must fail");
        assert!(matches!(e, AdapterError::Transport(_)), "got {e:?}");
        assert!(e.to_string().contains("definitely-not-a-real-port"));
    }

    #[tokio::test]
    async fn a_missing_port_gives_up_immediately_rather_than_waiting_out_a_timeout() {
        // A path that cannot open fails in `open(2)`, not by exhausting the
        // negotiation deadline. The old version of this code retried once per
        // candidate version and printed the same error four times.
        let start = std::time::Instant::now();
        let _ = connect("/dev/definitely-not-a-real-port", SerialSettings::FALLBACK).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should have given up immediately, took {:?}",
            start.elapsed()
        );
    }
}
