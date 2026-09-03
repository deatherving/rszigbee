//! Reachability: facts and mechanism in core, policy injected.
//!
//! An earlier design put all of availability in the MQTT layer, which would
//! have meant "you get no availability unless you run MQTT" — unacceptable for
//! embedded use, where an application has exactly the same need to know whether
//! a device is answering.
//!
//! The split:
//!
//! * **core owns the facts** ([`ReachabilityInfo`]) and the mechanism (one
//!   timer, one serialized probe queue). One scheduler, because two would fight
//!   over the radio and produce ping storms.
//! * **policy is injected** ([`ReachabilityPolicy`]) and decides *when*.
//!
//! The trait vocabulary is deliberately domain-neutral. An earlier draft had
//! `timeout()`, `should_probe()` and `next_check()`, which imported
//! `Zigbee2MQTT`'s framing: `timeout` conflates "how long until we call it
//! unreachable" with "how long until we probe", and presumes a timeout exists
//! at all. `rszigbee-mqtt` supplies a policy reproducing `Zigbee2MQTT`'s exact
//! semantics; that is one implementation, not the interface.

use std::time::{Duration, Instant, SystemTime};

use rszigbee_adapter::TxFailure;

use crate::device::DeviceInfo;

/// What we currently believe about a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reachability {
    /// Nothing has been heard yet — the state right after a restart. Distinct
    /// from `Unreachable` because "we have not looked" and "it did not answer"
    /// mean different things to anything acting on this.
    #[default]
    Unknown,
    /// Recent evidence the device is present.
    Reachable,
    /// Evidence it is not.
    Unreachable,
}

/// What a liveness probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    /// The device answered.
    Answered,
    /// It did not.
    Failed(TxFailure),
}

/// Why reachability changed, kept for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A frame arrived from the device.
    Traffic,
    /// A command to the device was acknowledged.
    CommandAcked,
    /// A command failed.
    CommandFailed(TxFailure),
    /// A probe answered or did not.
    Probe(ProbeResult),
    /// The policy re-evaluated on a timer with no new evidence.
    Elapsed,
}

/// The facts core tracks per device. Everything here is observed, never
/// inferred by policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReachabilityInfo {
    /// Current belief.
    pub state: Reachability,
    /// True for devices that sleep and cannot be probed on demand: battery end
    /// devices, and anything exposing `genPollCtrl`.
    pub is_sleepy: bool,
    /// When a frame was last received.
    pub last_seen: Option<SystemTime>,
    /// When a transmit to the device last succeeded.
    pub last_tx_ok: Option<Instant>,
    /// When a transmit last failed, and how.
    pub last_tx_err: Option<(Instant, TxFailure)>,
    /// When a probe last ran, and what it found.
    pub last_probe: Option<(Instant, ProbeResult)>,
    /// Consecutive failed probes. Policies use this to back off; core just
    /// counts.
    pub consecutive_probe_failures: u32,
}

impl Default for ReachabilityInfo {
    fn default() -> Self {
        Self {
            state: Reachability::Unknown,
            is_sleepy: false,
            last_seen: None,
            last_tx_ok: None,
            last_tx_err: None,
            last_probe: None,
            consecutive_probe_failures: 0,
        }
    }
}

impl ReachabilityInfo {
    /// How long since the last received frame, measured against `now`.
    #[must_use]
    pub fn silence(&self, now: SystemTime) -> Option<Duration> {
        self.last_seen.and_then(|t| now.duration_since(t).ok())
    }

    /// Records an inbound frame.
    pub fn record_traffic(&mut self, at: SystemTime) {
        self.last_seen = Some(at);
        self.consecutive_probe_failures = 0;
    }

    /// Records a transmit outcome.
    pub fn record_tx(&mut self, at: Instant, result: Result<(), TxFailure>) {
        match result {
            Ok(()) => self.last_tx_ok = Some(at),
            Err(e) => self.last_tx_err = Some((at, e)),
        }
    }

    /// Records a probe outcome.
    pub fn record_probe(&mut self, at: Instant, result: ProbeResult) {
        self.last_probe = Some((at, result));
        match result {
            ProbeResult::Answered => self.consecutive_probe_failures = 0,
            ProbeResult::Failed(_) => {
                self.consecutive_probe_failures = self.consecutive_probe_failures.saturating_add(1);
            }
        }
    }
}

/// Everything a policy is allowed to see.
#[derive(Debug, Clone, Copy)]
pub struct ReachabilityContext<'a> {
    /// The device.
    pub device: &'a DeviceInfo,
    /// Observed facts.
    pub current: &'a ReachabilityInfo,
    /// Monotonic now, for scheduling.
    pub now: Instant,
    /// Wall-clock now, for comparing against `last_seen`.
    pub wall_now: SystemTime,
}

/// A verdict plus what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    /// What we now believe.
    pub verdict: Reachability,
    /// When and how to look again.
    pub next: NextCheck,
}

/// What core should do next for this device.
///
/// These three variants are sufficient to express all of `Zigbee2MQTT`'s
/// availability behaviour without borrowing any of its vocabulary:
///
/// | Zigbee2MQTT behaviour | variant |
/// |---|---|
/// | active device, ping after the 10-minute timeout | `Probe { attempts: 2, allow_recovery }` |
/// | passive device, offline after 1500 minutes, never ping | `Reassess` |
/// | `pause_on_backoff_gt` reached: stop scheduling until traffic | `AwaitTraffic` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextCheck {
    /// Probe the device at this time.
    Probe {
        /// When.
        at: Instant,
        /// How many attempts before concluding failure.
        attempts: u8,
        /// Whether the adapter may attempt route repair. Upstream passes the
        /// equivalent as `ping(!available || i !== 2)`.
        allow_recovery: bool,
    },
    /// Do not probe; re-evaluate the verdict at this time. This is how a sleepy
    /// device is handled: probing it is pointless, but its silence still
    /// eventually means something.
    Reassess {
        /// When.
        at: Instant,
    },
    /// Schedule nothing. Wait for the device to speak first.
    AwaitTraffic,
}

/// Decides when to look and what to conclude.
pub trait ReachabilityPolicy: Send + Sync + 'static {
    /// Called on every relevant fact change and at the scheduled time.
    fn assess(&self, ctx: &ReachabilityContext<'_>) -> Assessment;
}

/// The default policy: conservative, and it never probes.
///
/// A device is `Reachable` while it has been heard from within `silence_limit`,
/// `Unreachable` after that, and `Unknown` until first contact. Probing is
/// opt-in because an embedded application may not want rszigbee generating
/// traffic it did not ask for. `rszigbee-mqtt` replaces this with a policy that
/// reproduces `Zigbee2MQTT`'s active/passive probing.
#[derive(Debug, Clone, Copy)]
pub struct SilencePolicy {
    /// How long a device may be silent before it is considered unreachable.
    pub silence_limit: Duration,
}

impl Default for SilencePolicy {
    fn default() -> Self {
        // 25 hours: upstream's passive-device default. Chosen because it is the
        // safe direction to be wrong in — a battery sensor that reports twice a
        // day must not be declared offline between reports.
        Self {
            silence_limit: Duration::from_secs(1500 * 60),
        }
    }
}

impl ReachabilityPolicy for SilencePolicy {
    fn assess(&self, ctx: &ReachabilityContext<'_>) -> Assessment {
        let Some(silence) = ctx.current.silence(ctx.wall_now) else {
            // Never heard from. Do not guess, and do not schedule: there is
            // nothing to time out from.
            return Assessment {
                verdict: Reachability::Unknown,
                next: NextCheck::AwaitTraffic,
            };
        };

        if silence < self.silence_limit {
            let remaining = self.silence_limit.saturating_sub(silence);
            Assessment {
                verdict: Reachability::Reachable,
                next: NextCheck::Reassess {
                    at: ctx.now + remaining,
                },
            }
        } else {
            Assessment {
                verdict: Reachability::Unreachable,
                next: NextCheck::AwaitTraffic,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceKind, PowerSource};
    use rszigbee_spec::ids::{Ieee, Nwk};

    fn device() -> DeviceInfo {
        DeviceInfo::new(Ieee::new(1), Nwk::new(2), DeviceKind::EndDevice)
    }

    fn ctx<'a>(
        dev: &'a DeviceInfo,
        info: &'a ReachabilityInfo,
        now: Instant,
        wall: SystemTime,
    ) -> ReachabilityContext<'a> {
        ReachabilityContext {
            device: dev,
            current: info,
            now,
            wall_now: wall,
        }
    }

    #[test]
    fn a_device_never_heard_from_is_unknown_not_unreachable() {
        // Reporting a freshly restored device as offline before it has had a
        // chance to speak produces a wave of false offline notifications.
        let d = device();
        let info = ReachabilityInfo::default();
        let a = SilencePolicy::default().assess(&ctx(&d, &info, Instant::now(), SystemTime::now()));
        assert_eq!(a.verdict, Reachability::Unknown);
        assert_eq!(a.next, NextCheck::AwaitTraffic);
    }

    #[test]
    fn recent_traffic_means_reachable_and_schedules_the_remaining_time() {
        let d = device();
        let wall = SystemTime::now();
        let mut info = ReachabilityInfo::default();
        info.record_traffic(wall - Duration::from_secs(60));

        let policy = SilencePolicy {
            silence_limit: Duration::from_secs(600),
        };
        let now = Instant::now();
        let a = policy.assess(&ctx(&d, &info, now, wall));
        assert_eq!(a.verdict, Reachability::Reachable);
        match a.next {
            NextCheck::Reassess { at } => {
                // ~540 s remaining; allow slack for clock granularity.
                let delta = at.saturating_duration_since(now);
                assert!(delta <= Duration::from_secs(540), "{delta:?}");
                assert!(delta >= Duration::from_secs(535), "{delta:?}");
            }
            other => panic!("expected Reassess, got {other:?}"),
        }
    }

    #[test]
    fn prolonged_silence_means_unreachable() {
        let d = device();
        let wall = SystemTime::now();
        let mut info = ReachabilityInfo::default();
        info.record_traffic(wall - Duration::from_secs(7200));

        let policy = SilencePolicy {
            silence_limit: Duration::from_secs(600),
        };
        let a = policy.assess(&ctx(&d, &info, Instant::now(), wall));
        assert_eq!(a.verdict, Reachability::Unreachable);
    }

    #[test]
    fn the_default_limit_does_not_declare_sleepy_devices_offline_too_early() {
        // The specific failure this guards: a temperature sensor reporting once
        // an hour must stay online. Upstream's default is 1500 minutes for
        // exactly this reason.
        let policy = SilencePolicy::default();
        assert!(policy.silence_limit >= Duration::from_secs(24 * 3600));
    }

    #[test]
    fn probe_failures_accumulate_and_traffic_clears_them() {
        let mut info = ReachabilityInfo::default();
        let t = Instant::now();
        info.record_probe(t, ProbeResult::Failed(TxFailure::NoAck));
        info.record_probe(t, ProbeResult::Failed(TxFailure::NoAck));
        assert_eq!(info.consecutive_probe_failures, 2);

        info.record_probe(t, ProbeResult::Answered);
        assert_eq!(info.consecutive_probe_failures, 0);

        info.record_probe(t, ProbeResult::Failed(TxFailure::NoRoute));
        assert_eq!(info.consecutive_probe_failures, 1);
        // Real traffic is stronger evidence than a probe and resets the count.
        info.record_traffic(SystemTime::now());
        assert_eq!(info.consecutive_probe_failures, 0);
    }

    #[test]
    fn a_custom_policy_can_probe_which_is_the_point_of_the_seam() {
        // Proves the trait can express probing without core knowing the rules.
        struct Eager;
        impl ReachabilityPolicy for Eager {
            fn assess(&self, ctx: &ReachabilityContext<'_>) -> Assessment {
                let stale = ctx
                    .current
                    .silence(ctx.wall_now)
                    .is_none_or(|s| s > Duration::from_secs(30));
                if stale && !ctx.current.is_sleepy {
                    Assessment {
                        verdict: ctx.current.state,
                        next: NextCheck::Probe {
                            at: ctx.now,
                            attempts: 2,
                            allow_recovery: false,
                        },
                    }
                } else {
                    Assessment {
                        verdict: Reachability::Reachable,
                        next: NextCheck::Reassess {
                            at: ctx.now + Duration::from_secs(30),
                        },
                    }
                }
            }
        }

        let mut d = device();
        d.power_source = PowerSource::Mains;
        let info = ReachabilityInfo::default();
        let a = Eager.assess(&ctx(&d, &info, Instant::now(), SystemTime::now()));
        assert!(matches!(
            a.next,
            NextCheck::Probe {
                attempts: 2,
                allow_recovery: false,
                ..
            }
        ));
    }

    #[test]
    fn a_sleepy_device_is_never_probed_by_a_policy_that_checks_the_flag() {
        struct Eager;
        impl ReachabilityPolicy for Eager {
            fn assess(&self, ctx: &ReachabilityContext<'_>) -> Assessment {
                let next = if ctx.current.is_sleepy {
                    NextCheck::Reassess {
                        at: ctx.now + Duration::from_secs(3600),
                    }
                } else {
                    NextCheck::Probe {
                        at: ctx.now,
                        attempts: 1,
                        allow_recovery: true,
                    }
                };
                Assessment {
                    verdict: ctx.current.state,
                    next,
                }
            }
        }
        let d = device();
        let info = ReachabilityInfo {
            is_sleepy: true,
            ..ReachabilityInfo::default()
        };
        let a = Eager.assess(&ctx(&d, &info, Instant::now(), SystemTime::now()));
        assert!(matches!(a.next, NextCheck::Reassess { .. }));
    }
}
