//! A cloneable command interface over a single-owner [`Ncp`].
//!
//! `rsezsp` deliberately hands out one owner: EZSP allows one command in
//! flight at a time, and a `&mut` handle makes that a compile-time fact rather
//! than a convention. The adapter needs something different — several call
//! sites issuing commands, plus callbacks arriving on their own while nothing
//! is being asked for.
//!
//! Reconciled here rather than in `rsezsp`, because concurrency policy belongs
//! to the application. One task owns the `Ncp`; everyone else sends it a
//! request and waits for a reply. Commands serialise, which is exactly the
//! protocol's own constraint, and callbacks are forwarded as they arrive.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use rsezsp::Ncp;
use rsezsp::ezsp::EzspError;
use rsezsp::ezsp::callback::Callback;
use rsezsp::ezsp::command::Command;
use rsezsp::transport::Transport;
use rszigbee_adapter::AdapterError;
use tokio::sync::{mpsc, oneshot};

/// Depth of the request channel.
///
/// Bounded: an unbounded channel turns a slow NCP into unbounded memory growth
/// rather than backpressure, and backpressure is the honest signal here.
const REQUESTS: usize = 64;

/// How long the owning task waits for callbacks when no command is queued.
///
/// The cost of getting this wrong is only latency, in one direction each way: a
/// long idle poll delays a command that arrives mid-poll, and a short one wakes
/// the task more often. It is short because bringup issues dozens of commands
/// in sequence and a stall on each would be felt; the read itself blocks, so
/// waking often is cheap.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// A request for the task that owns the NCP.
///
/// Boxed as a trait object so one variant covers every command: the alternative
/// is an enum with a variant per command, which has to be edited every time a
/// command is added.
trait Call<T: Transport>: Send {
    /// Runs the command and answers the caller.
    fn run<'a>(
        self: Box<Self>,
        ncp: &'a mut Ncp<T>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// A command paired with the channel its answer goes back on.
struct Invocation<C: Command> {
    command: C,
    reply: oneshot::Sender<Result<C::Response, EzspError>>,
}

impl<T, C> Call<T> for Invocation<C>
where
    T: Transport + Send,
    C: Command + Send + 'static,
    C::Response: Send,
{
    fn run<'a>(
        self: Box<Self>,
        ncp: &'a mut Ncp<T>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let result = ncp.command(self.command).await;
            // A dropped receiver means the caller gave up. The command has
            // already been sent to the NCP, so there is nothing to undo and
            // nothing to report.
            drop(self.reply.send(result));
        })
    }
}

/// A cloneable handle to the NCP.
#[derive(Clone)]
pub struct Connection {
    requests: mpsc::Sender<Box<dyn Call<rsezsp::transport::serial::SerialTransport>>>,
    /// The negotiated EZSP version, so callers can ask without a round trip.
    version: u8,
    /// The NCP firmware's own version number.
    stack_version: u16,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because the request channel carries boxed trait objects,
        // which have no `Debug`. The two version numbers are what anyone
        // reading a log actually wants.
        f.debug_struct("Connection")
            .field("version", &self.version)
            .field(
                "stack_version",
                &format_args!("{:#06x}", self.stack_version),
            )
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Takes ownership of an `Ncp` and returns a handle to it, plus the stream
    /// of callbacks it produces.
    ///
    /// The task exits when every handle is dropped, which drops the `Ncp` and
    /// closes the serial port.
    pub fn spawn(
        mut ncp: Ncp<rsezsp::transport::serial::SerialTransport>,
    ) -> (Self, mpsc::Receiver<Callback>) {
        let (requests_tx, mut requests_rx) =
            mpsc::channel::<Box<dyn Call<rsezsp::transport::serial::SerialTransport>>>(REQUESTS);
        let (callbacks_tx, callbacks_rx) = mpsc::channel(REQUESTS);

        let version = ncp.version().raw();
        let stack_version = ncp.stack_version();

        tokio::spawn(async move {
            loop {
                // Queued work first. Only when there is none is it worth
                // spending time reading for callbacks, and checking without
                // waiting keeps a run of sequential commands at full speed.
                let collected = match requests_rx.try_recv() {
                    Ok(call) => {
                        call.run(&mut ncp).await;
                        // A command's read loop collects any callbacks that
                        // arrived while it was waiting for its response.
                        ncp.take_callbacks()
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        // Nothing to do: give the NCP a chance to speak. A
                        // failure here is the port going away, which the next
                        // command will report properly.
                        //
                        // `poll` *returns* what it collected, having drained it
                        // already -- calling `take_callbacks` afterwards yields
                        // an empty vector. Discarding this value silently drops
                        // every callback that arrives while no command is in
                        // flight, which is most of the interesting ones: a ZDO
                        // response, an attribute report, a device joining.
                        match ncp.poll(IDLE_POLL).await {
                            Ok(callbacks) => callbacks,
                            Err(_) => break,
                        }
                    }
                };

                for callback in collected {
                    if callbacks_tx.send(callback).await.is_err() {
                        // Nobody is listening for callbacks any more. Commands
                        // may still be in flight, so this is not a reason to
                        // stop.
                        break;
                    }
                }
            }
            tracing::debug!("EZSP connection task finished");
        });

        (
            Self {
                requests: requests_tx,
                version,
                stack_version,
            },
            callbacks_rx,
        )
    }

    /// The negotiated EZSP protocol version.
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    /// The NCP firmware's own version number, such as `0x7440` for
    /// `EmberZNet` 7.4.4.
    #[must_use]
    pub const fn stack_version(&self) -> u16 {
        self.stack_version
    }

    /// Sends one command and waits for its answer.
    ///
    /// # Errors
    ///
    /// [`AdapterError::Transport`] if the owning task has stopped, and
    /// whatever the NCP reported otherwise.
    pub async fn command<C>(&self, command: C) -> Result<C::Response, EzspError>
    where
        C: Command + Send + 'static,
        C::Response: Send,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.requests
            .send(Box::new(Invocation {
                command,
                reply: reply_tx,
            }))
            .await
            .map_err(|_| EzspError::NotConnected)?;

        reply_rx.await.map_err(|_| EzspError::NotConnected)?
    }
}

/// Turns an EZSP failure into the adapter's error type, with context.
///
/// Kept as a free function rather than a `From` impl so every call site has to
/// say what it was doing: "the NCP returned FAIL" is not a diagnosis.
pub fn context(what: &str, error: &EzspError) -> AdapterError {
    AdapterError::Transport(format!("{what}: {error}"))
}

/// Turns a non-success status into an adapter error, with context.
///
/// The NCP answering is not the same as the NCP agreeing. Most commands return
/// a status that has to be looked at; ignoring it produces a bring-up that
/// reports success while nothing was configured.
///
/// # Errors
///
/// [`AdapterError::Transport`] naming `what` and the status, for anything but
/// success.
pub fn check(what: &str, status: rsezsp::SlStatus) -> Result<(), AdapterError> {
    if status.is_ok() {
        Ok(())
    } else {
        Err(AdapterError::Transport(format!("{what}: {status}")))
    }
}
