//! Connectivity monitoring with anti-flapping logic for the GroupMe desktop client.
//!
//! Hard requirement: a 2-second network blip must NOT yank the user out of a
//! conversation. Window swaps are jarring and lose scroll position, so this state
//! machine is deliberately reluctant to declare `Offline` and eager to return to
//! `Online`.
//!
//! Anti-flapping design
//! --------------------
//! - `Degraded` absorbs early failures. The UI may show a subtle indicator here
//!   but must not swap the webview until consecutive failures reach the
//!   `failures_to_offline` threshold.
//! - Recovery is optimistic: a single probe success immediately returns to
//!   `Online` (even from `Degraded`). There is no cost to being wrong in the
//!   Online direction.
//! - State transitions are broadcast only when the value actually changes, so
//!   subscribers never receive duplicate events.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, warn};
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Connectivity {
    Online,
    /// At least one probe failed but we have not hit the offline threshold.
    /// The UI does not swap on this — it may show a subtle indicator.
    Degraded,
    Offline,
}

// Design choice — option (a): native async fn in traits (AFIT, stabilised in
// Rust 1.75) with `ConnectivityMonitor` generic over `P: ConnectivityProbe`.
//
// This yields zero-cost monomorphization: the compiler stamps out exactly one
// copy for `HttpProbe` in production and one for `FakeProbe` in tests. We
// never need `dyn ConnectivityProbe` because we only ever have one concrete
// probe type at runtime, so the non-dyn-compatibility of AFIT is irrelevant.
pub trait ConnectivityProbe: Send + Sync + 'static {
    fn probe(&self) -> impl std::future::Future<Output = bool> + Send;
}

// ---------------------------------------------------------------------------
// HttpProbe
// ---------------------------------------------------------------------------

const DEFAULT_PROBE_URL: &str = "https://api.groupme.com/v3/users/me";

/// 5-second timeout applied to the full request lifecycle, not just connect.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Real probe: hits a cheap GroupMe endpoint.
///
/// Any HTTP response — including 401 or 404 — counts as reachable. We are
/// testing the network path, not credentials. A revoked token still means the
/// user is online. Only transport-level errors (DNS failure, connection refused,
/// timeout) count as a failure.
pub struct HttpProbe {
    client: reqwest::Client,
    url: String,
}

impl HttpProbe {
    pub fn new() -> Self {
        Self::with_url(DEFAULT_PROBE_URL)
    }

    pub fn with_url(url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            // Only fails when no TLS backend is available — unrecoverable at
            // init time, so a panic here is appropriate.
            .expect("reqwest client construction failed (no TLS backend)");
        Self { client, url: url.into() }
    }
}

impl Default for HttpProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectivityProbe for HttpProbe {
    async fn probe(&self) -> bool {
        match self.client.get(&self.url).send().await {
            Ok(_) => {
                debug!("connectivity probe: reachable");
                true
            }
            Err(e) => {
                warn!("connectivity probe: unreachable — {e}");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MonitorConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct MonitorConfig {
    /// Consecutive failures before declaring Offline. Default 3.
    pub failures_to_offline: u32,
    /// Consecutive successes before returning to Online from Offline. Default 1
    /// — recovery should be immediate; there is no cost to being optimistic here.
    pub successes_to_online: u32,
    /// Interval between probes while Online. Default 30 s.
    pub interval_online: Duration,
    /// Interval while Degraded/Offline — poll faster so recovery is quick. Default 5 s.
    pub interval_offline: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            failures_to_offline: 3,
            successes_to_online: 1,
            interval_online: Duration::from_secs(30),
            interval_offline: Duration::from_secs(5),
        }
    }
}

// ---------------------------------------------------------------------------
// ConnectivityMonitor
// ---------------------------------------------------------------------------

struct MonitorInner {
    consecutive_failures: u32,
    /// Tracked only when recovering from Offline with successes_to_online > 1.
    /// Reset to 0 on any failure.
    consecutive_successes: u32,
}

pub struct ConnectivityMonitor<P: ConnectivityProbe> {
    probe: P,
    config: MonitorConfig,
    /// watch channel carries the authoritative current state and notifies
    /// subscribers on change. Using watch (not broadcast) is deliberate: new
    /// subscribers immediately see the latest value, there is no backlog, and
    /// the sender can also read the current value via `borrow()`.
    tx: watch::Sender<Connectivity>,
    inner: Mutex<MonitorInner>,
}

impl<P: ConnectivityProbe> ConnectivityMonitor<P> {
    pub fn new(probe: P) -> Self {
        Self::with_config(probe, MonitorConfig::default())
    }

    pub fn with_config(probe: P, config: MonitorConfig) -> Self {
        let (tx, _seed_rx) = watch::channel(Connectivity::Online);
        Self {
            probe,
            config,
            tx,
            inner: Mutex::new(MonitorInner {
                consecutive_failures: 0,
                consecutive_successes: 0,
            }),
        }
    }

    /// Current state. Cheap, non-blocking.
    pub fn state(&self) -> Connectivity {
        *self.tx.borrow()
    }

    /// Subscribe to state transitions. The receiver is notified only when the
    /// state actually changes — not on no-op probes.
    pub fn subscribe(&self) -> watch::Receiver<Connectivity> {
        self.tx.subscribe()
    }

    /// Force an immediate probe and state update; returns the new state.
    ///
    /// Call this when the webview fires a browser `online`/`offline` event so
    /// the monitor reacts instantly rather than waiting for the next scheduled
    /// tick.
    pub async fn poll_now(&self) -> Connectivity {
        // Probe before acquiring the lock — the real probe can block for up
        // to PROBE_TIMEOUT and we must not hold the lock across that await.
        let success = self.probe.probe().await;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_result(&mut inner, success)
    }

    /// Runs the probe loop forever. Spawn this as a Tokio task.
    pub async fn run(self: Arc<Self>) {
        loop {
            let interval = match self.state() {
                Connectivity::Online => self.config.interval_online,
                _ => self.config.interval_offline,
            };
            tokio::time::sleep(interval).await;
            self.poll_now().await;
        }
    }

    // Advance the state machine with one probe result.
    // Must be called with `inner` already locked by the caller.
    fn apply_result(&self, inner: &mut MonitorInner, success: bool) -> Connectivity {
        let new_state = if success {
            inner.consecutive_failures = 0;
            inner.consecutive_successes += 1;
            // successes_to_online only gates recovery from Offline.
            // From Degraded, a single success is enough — no penalty for
            // optimism when the network is flapping.
            let current = *self.tx.borrow();
            if current == Connectivity::Offline
                && inner.consecutive_successes < self.config.successes_to_online
            {
                Connectivity::Offline
            } else {
                Connectivity::Online
            }
        } else {
            inner.consecutive_successes = 0;
            inner.consecutive_failures += 1;
            if inner.consecutive_failures >= self.config.failures_to_offline {
                Connectivity::Offline
            } else {
                // At least one failure but below the Offline threshold.
                Connectivity::Degraded
            }
        };

        // `send_if_modified`, not `send`.
        //
        // `watch::Sender::send` fails when every receiver has been dropped, and
        // critically it does NOT store the value in that case. With no live
        // subscriber every transition was silently discarded and `state()` kept
        // reporting the initial value forever.
        //
        // `send_if_modified` always writes, notifies only on a real change, and
        // does both under one lock — which also closes the read-then-write race
        // that a separate `borrow()` + `send()` pair leaves open.
        self.tx.send_if_modified(|current| {
            if *current == new_state {
                false
            } else {
                *current = new_state;
                true
            }
        });
        new_state
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    // -----------------------------------------------------------------------
    // FakeProbe
    // -----------------------------------------------------------------------

    /// Hand the scheduler enough slots for a spawned task to get through a
    /// probe and a state write. One `yield_now` covers a single await point;
    /// the probe loop has several, so a single yield makes the test racy.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    struct FakeProbe {
        results: Mutex<VecDeque<bool>>,
        call_count: Arc<AtomicUsize>,
    }

    impl FakeProbe {
        /// Returns `(probe, shared_call_counter)`.
        /// Results beyond the script return `true` (optimistic default).
        fn with_script(
            results: impl IntoIterator<Item = bool>,
        ) -> (Self, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            let probe = Self {
                results: Mutex::new(results.into_iter().collect()),
                call_count: count.clone(),
            };
            (probe, count)
        }

        /// All probes fail (pre-filled; effectively infinite for test durations).
        fn always_failing() -> (Self, Arc<AtomicUsize>) {
            Self::with_script(std::iter::repeat(false).take(10_000))
        }
    }

    impl ConnectivityProbe for FakeProbe {
        async fn probe(&self) -> bool {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or(true)
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn monitor_from_script(
        results: impl IntoIterator<Item = bool>,
    ) -> Arc<ConnectivityMonitor<FakeProbe>> {
        let (probe, _) = FakeProbe::with_script(results);
        Arc::new(ConnectivityMonitor::new(probe))
    }

    // -----------------------------------------------------------------------
    // State-machine tests (no time dependency)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn starts_online() {
        let m = monitor_from_script([]);
        assert_eq!(m.state(), Connectivity::Online);
    }

    #[tokio::test]
    async fn single_failure_is_degraded_not_offline() {
        // THE key anti-flapping test: one transient failure must not declare Offline.
        let m = monitor_from_script([false]);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Degraded);
    }

    #[tokio::test]
    async fn three_consecutive_failures_declare_offline() {
        let m = monitor_from_script([false, false, false]);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Degraded);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Degraded);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Offline);
    }

    #[tokio::test]
    async fn success_after_failures_resets_counter() {
        // ff → success → f should land on Degraded, proving the counter was
        // reset. If the counter were not reset the last failure would be the
        // third consecutive and we would be Offline.
        let m = monitor_from_script([false, false, true, false]);
        m.poll_now().await; // → Degraded
        m.poll_now().await; // → Degraded
        m.poll_now().await; // → Online  (success, counter reset)
        assert_eq!(m.state(), Connectivity::Online);
        m.poll_now().await; // → Degraded (single failure; counter was reset)
        assert_eq!(m.state(), Connectivity::Degraded);
    }

    #[tokio::test]
    async fn recovery_from_offline_on_first_success() {
        let m = monitor_from_script([false, false, false, true]);
        m.poll_now().await; // Degraded
        m.poll_now().await; // Degraded
        m.poll_now().await; // Offline
        assert_eq!(m.state(), Connectivity::Offline);
        m.poll_now().await; // → Online immediately (successes_to_online default = 1)
        assert_eq!(m.state(), Connectivity::Online);
    }

    #[tokio::test]
    async fn subscribe_receives_transitions_and_no_duplicates() {
        let m = monitor_from_script([false, false, false, true]);
        let mut rx = m.subscribe();
        assert_eq!(*rx.borrow(), Connectivity::Online);

        m.poll_now().await; // Online → Degraded
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Connectivity::Degraded);

        // Second failure keeps us at Degraded — must NOT produce a new event.
        m.poll_now().await;
        assert!(
            !rx.has_changed().unwrap(),
            "duplicate Degraded event must not be broadcast"
        );

        m.poll_now().await; // → Offline
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Connectivity::Offline);

        m.poll_now().await; // → Online
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Connectivity::Online);
    }

    #[tokio::test]
    async fn poll_now_updates_state_immediately() {
        let m = monitor_from_script([false]);
        assert_eq!(m.state(), Connectivity::Online);
        let returned = m.poll_now().await;
        assert_eq!(returned, Connectivity::Degraded);
        assert_eq!(m.state(), Connectivity::Degraded);
    }

    // -----------------------------------------------------------------------
    // Timing test — tokio::time::pause + advance; zero real-time elapsed.
    // -----------------------------------------------------------------------

    /// Prove the probe interval shortens when Offline.
    ///
    /// With `failures_to_offline = 1`, the first scheduled probe (at
    /// `interval_online = 30 s`) immediately declares Offline. We then advance
    /// six `interval_offline = 5 s` windows and assert ≥ 5 additional probes
    /// fired. If the interval had not shortened, at most 1 probe would have
    /// fired in that same 30 s.
    #[tokio::test(start_paused = true)]
    async fn offline_interval_is_faster_than_online_interval() {
        let config = MonitorConfig {
            failures_to_offline: 1,
            successes_to_online: 1,
            interval_online: Duration::from_secs(30),
            interval_offline: Duration::from_secs(5),
        };

        let (probe, count) = FakeProbe::always_failing();
        let m = Arc::new(ConnectivityMonitor::with_config(probe, config));
        let m2 = m.clone();
        tokio::spawn(async move { m2.run().await });

        // Let the spawned loop reach its first `sleep` and register a timer.
        // `tokio::spawn` does not run the task until the current one yields, so
        // advancing the clock first would move `now` past a timer that has not
        // been created yet — the loop would then sleep a full interval from the
        // NEW now and never fire.
        settle().await;

        // Advance past the Online interval -> first probe fires -> Offline.
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert_eq!(m.state(), Connectivity::Offline);
        let probes_at_offline = count.load(Ordering::SeqCst);

        // Now advance six Offline intervals (5 s x 6 = 30 s).
        // At the Online interval (30 s) we would see <= 1 extra probe.
        // At the Offline interval (5 s) we expect ~6.
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(5)).await;
            settle().await;
        }

        let extra = count.load(Ordering::SeqCst) - probes_at_offline;
        assert!(
            extra >= 5,
            "expected ≥5 probes in 30 s at 5 s offline interval, got {extra}"
        );
    }
}
