// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! When the daemon may stop by itself.
//!
//! The daemon stops once it has been idle for the idle period: no KNX session
//! is configured, no control request arrived, and no long-lived client holds a
//! lease. A configured KNX session holds the daemon until it is disconnected,
//! because capturing is what the daemon is for. A lease is how a long-lived
//! client (the window, the terminal monitor, an MCP server, `monitor` or
//! `follow`) says it is still there, since every control request opens a
//! fresh connection and there is no standing connection to watch. A REST
//! listener alone does not hold the daemon.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The idle period when nothing sets another.
pub const DEFAULT_IDLE: Duration = Duration::from_mins(15);

/// How long a lease lasts without renewal.
pub const LEASE_TTL: Duration = Duration::from_mins(3);

/// How often a long-lived client renews its lease.
pub const LEASE_RENEW: Duration = Duration::from_mins(1);

/// A length of time as people write it: `90s`, `15m`, `2h`, or `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeout(pub Duration);

impl Default for IdleTimeout {
    fn default() -> Self {
        Self(DEFAULT_IDLE)
    }
}

impl FromStr for IdleTimeout {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text == "0" {
            return Ok(Self(Duration::ZERO));
        }
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("'{text}' needs a unit: s, m or h, as in 15m"))?;
        let (number, unit) = text.split_at(split);
        let number: u64 = number
            .parse()
            .map_err(|_| format!("'{text}' does not start with a number"))?;
        let seconds = match unit {
            "s" => Some(number),
            "m" => number.checked_mul(60),
            "h" => number.checked_mul(3600),
            _ => return Err(format!("'{unit}' is not a unit; use s, m or h, as in 15m")),
        }
        .ok_or_else(|| format!("'{text}' is too long"))?;
        Ok(Self(Duration::from_secs(seconds)))
    }
}

impl fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_secs() {
            0 => f.write_str("0"),
            s if s % 3600 == 0 => write!(f, "{}h", s / 3600),
            s if s % 60 == 0 => write!(f, "{}m", s / 60),
            s => write!(f, "{s}s"),
        }
    }
}

impl Serialize for IdleTimeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IdleTimeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The kind of long-lived client holding a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    /// The desktop window.
    Gui,
    /// The terminal monitor.
    Tui,
    /// An MCP server.
    Mcp,
    /// `devknx monitor`.
    Monitor,
    /// `devknx follow`.
    Follow,
}

impl fmt::Display for LeaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gui => "window",
            Self::Tui => "terminal monitor",
            Self::Mcp => "MCP server",
            Self::Monitor => "monitor",
            Self::Follow => "follow",
        })
    }
}

/// Where the idle period in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleSource {
    /// The built-in default.
    Default,
    /// `idle_timeout` in `settings.json`.
    Settings,
    /// `devknx daemon --idle-timeout` when the daemon was started.
    StartFlag,
    /// Changed on the running daemon.
    Runtime,
}

impl fmt::Display for IdleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "built-in default",
            Self::Settings => "settings",
            Self::StartFlag => "start flag",
            Self::Runtime => "changed at runtime",
        })
    }
}

/// When the daemon would stop by itself, and what holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleState {
    /// The idle period in seconds; `0` means never.
    pub idle_seconds: u64,
    /// Where that period came from.
    pub source: IdleSource,
    /// Seconds until an idle stop, when one is due at all: `None` while a
    /// session or a lease holds the daemon, or the period is `0`.
    pub stops_in_seconds: Option<u64>,
    /// The number of configured KNX sessions.
    pub sessions: usize,
    /// The kinds of long-lived client attached, one entry per lease.
    pub leases: Vec<LeaseKind>,
}

/// Where the current time comes from, so tests need not wait.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct Inner {
    period: Duration,
    source: IdleSource,
    last_activity: Instant,
    leases: HashMap<String, (LeaseKind, Instant)>,
}

/// The daemon's idle timer and its leases.
pub struct Lifecycle {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

impl fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lifecycle").finish_non_exhaustive()
    }
}

impl Lifecycle {
    /// A timer that starts now.
    #[must_use]
    pub fn new(period: Duration, source: IdleSource, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            clock,
            inner: Mutex::new(Inner {
                period,
                source,
                last_activity: now,
                leases: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Something happened: a request arrived, or a session ended.
    pub fn touch(&self) {
        let now = self.clock.now();
        self.lock().last_activity = now;
    }

    /// Create or extend a lease. A renewal creates the lease when the daemon
    /// does not know it, so a client registers again with a new daemon.
    pub fn renew(&self, client: &str, kind: LeaseKind) {
        let expires = self.clock.now() + LEASE_TTL;
        self.lock()
            .leases
            .insert(client.to_owned(), (kind, expires));
    }

    /// End a lease. Releasing counts as activity: the idle period starts
    /// when the last client leaves, not when it last renewed.
    pub fn release(&self, client: &str) {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.remove(client);
        inner.last_activity = now;
    }

    /// Change the idle period, recording where the new value came from.
    pub fn set_period(&self, period: Duration, source: IdleSource) {
        let mut inner = self.lock();
        inner.period = period;
        inner.source = source;
    }

    /// The state as a client sees it, with `sessions` configured KNX
    /// sessions. Lapsed leases are dropped first.
    #[must_use]
    pub fn state(&self, sessions: usize) -> LifecycleState {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.retain(|_, (_, expires)| *expires > now);
        let mut leases: Vec<LeaseKind> = inner.leases.values().map(|(kind, _)| *kind).collect();
        leases.sort();
        let stops_in_seconds =
            (inner.period > Duration::ZERO && leases.is_empty() && sessions == 0).then(|| {
                inner
                    .period
                    .saturating_sub(now.saturating_duration_since(inner.last_activity))
                    .as_secs()
            });
        LifecycleState {
            idle_seconds: inner.period.as_secs(),
            source: inner.source,
            stops_in_seconds,
            sessions,
            leases,
        }
    }

    /// Whether the daemon, with `sessions` configured KNX sessions, should
    /// stop now.
    #[must_use]
    pub fn idle_expired(&self, sessions: usize) -> bool {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.retain(|_, (_, expires)| *expires > now);
        inner.period > Duration::ZERO
            && sessions == 0
            && inner.leases.is_empty()
            && now.saturating_duration_since(inner.last_activity) >= inner.period
    }
}

/// A lease a long-lived client holds on the daemon while it runs.
///
/// A thread renews it every [`LEASE_RENEW`] and releases it when the keeper is
/// dropped. A process that dies without dropping it loses the lease after
/// [`LEASE_TTL`].
///
/// A renewal never starts a daemon. It goes to whatever daemon is running, and
/// to none if none is: otherwise an open window would bring back a daemon that
/// was stopped on purpose. The next real operation starts one, and the next
/// renewal registers the lease with it.
pub struct LeaseKeeper {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LeaseKeeper {
    /// Hold a lease of this kind on the running daemon.
    #[must_use]
    pub fn start(kind: LeaseKind) -> Self {
        Self::start_every(kind, LEASE_RENEW)
    }

    /// The same, renewing at another interval, for tests.
    #[must_use]
    pub fn start_every(kind: LeaseKind, every: Duration) -> Self {
        use crate::control::{ControlClient, ControlRequest};

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let id = format!("{kind:?}-{}-{stamp}", std::process::id()).to_lowercase();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("devknx-lease".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                loop {
                    let _ = runtime.block_on(ControlClient::request_existing(
                        ControlRequest::RenewLease {
                            client: id.clone(),
                            kind,
                        },
                    ));
                    match stopped.recv_timeout(every) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = runtime.block_on(ControlClient::request_existing(
                    ControlRequest::ReleaseLease { client: id },
                ));
            })
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        // Closing the channel ends the renewal loop, which then releases.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the test moves by hand.
    #[derive(Debug)]
    struct ManualClock(Mutex<Instant>);

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Instant::now())))
        }

        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    /// One second less than `period`.
    fn just_under(period: Duration) -> Duration {
        period.checked_sub(Duration::from_secs(1)).unwrap()
    }

    fn lifecycle(clock: &Arc<ManualClock>) -> Lifecycle {
        Lifecycle::new(DEFAULT_IDLE, IdleSource::Default, clock.clone())
    }

    #[test]
    fn idle_timeouts_parse_and_print_as_written() {
        for (text, seconds) in [("0", 0), ("90s", 90), ("15m", 900), ("2h", 7200)] {
            let parsed: IdleTimeout = text.parse().unwrap();
            assert_eq!(parsed.0.as_secs(), seconds);
            assert_eq!(parsed.to_string(), text);
        }
        for bad in ["15", "m", "15d", "-1m", ""] {
            assert!(bad.parse::<IdleTimeout>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_daemon_stops_after_the_idle_period_and_not_before() {
        let clock = ManualClock::new();
        let lifecycle = lifecycle(&clock);
        clock.advance(just_under(DEFAULT_IDLE));
        assert!(!lifecycle.idle_expired(0));
        assert_eq!(lifecycle.state(0).stops_in_seconds, Some(1));
        clock.advance(Duration::from_secs(1));
        assert!(lifecycle.idle_expired(0));
    }

    #[test]
    fn a_request_starts_the_idle_period_again() {
        let clock = ManualClock::new();
        let lifecycle = lifecycle(&clock);
        clock.advance(just_under(DEFAULT_IDLE));
        lifecycle.touch();
        clock.advance(Duration::from_secs(2));
        assert!(!lifecycle.idle_expired(0));
    }

    #[test]
    fn a_configured_session_holds_the_daemon() {
        let clock = ManualClock::new();
        let lifecycle = lifecycle(&clock);
        clock.advance(DEFAULT_IDLE * 10);
        assert!(!lifecycle.idle_expired(1));
        assert_eq!(lifecycle.state(1).stops_in_seconds, None);
        // Counter-probe: the same moment without a session stops.
        assert!(lifecycle.idle_expired(0));
    }

    #[test]
    fn a_lease_holds_until_released_or_lapsed() {
        let clock = ManualClock::new();
        let period = Duration::from_mins(1);
        let lifecycle = Lifecycle::new(period, IdleSource::Default, clock.clone());
        // Renewed within its lifetime, the lease holds well past the period.
        lifecycle.renew("gui-1", LeaseKind::Gui);
        clock.advance(just_under(LEASE_TTL));
        lifecycle.renew("gui-1", LeaseKind::Gui);
        clock.advance(just_under(LEASE_TTL));
        assert!(!lifecycle.idle_expired(0));
        assert_eq!(lifecycle.state(0).leases, vec![LeaseKind::Gui]);

        // Released: the idle period starts at the release.
        lifecycle.release("gui-1");
        clock.advance(just_under(period));
        assert!(!lifecycle.idle_expired(0));
        clock.advance(Duration::from_secs(1));
        assert!(lifecycle.idle_expired(0));

        // Lapsed: a client that died without releasing stops holding.
        let lifecycle = lifecycle_after_lapse(&clock);
        let leases = lifecycle.state(0).leases;
        assert!(leases.is_empty(), "{leases:?}");
        assert!(lifecycle.idle_expired(0));
    }

    fn lifecycle_after_lapse(clock: &Arc<ManualClock>) -> Lifecycle {
        let lifecycle = Lifecycle::new(Duration::from_secs(1), IdleSource::Default, clock.clone());
        lifecycle.renew("tui-1", LeaseKind::Tui);
        clock.advance(LEASE_TTL);
        lifecycle
    }

    #[test]
    fn a_period_of_zero_never_stops() {
        let clock = ManualClock::new();
        let lifecycle = Lifecycle::new(Duration::ZERO, IdleSource::StartFlag, clock.clone());
        clock.advance(DEFAULT_IDLE * 100);
        assert!(!lifecycle.idle_expired(0));
        assert_eq!(lifecycle.state(0).stops_in_seconds, None);
        lifecycle.set_period(Duration::from_secs(1), IdleSource::Runtime);
        assert!(lifecycle.idle_expired(0));
        assert_eq!(lifecycle.state(0).source, IdleSource::Runtime);
    }
}
