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
//!
//! Testing override
//! ----------------
//! [`set_forced_offline`] pins the reported state to `Offline` whatever the
//! probe says, so offline mode can be exercised without dropping the network.
//! It is process-global, never persisted, and does not stop the probe loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};
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

// ---------------------------------------------------------------------------
// Forced-offline override
// ---------------------------------------------------------------------------

/// Process-global rather than a monitor field because the tray flips it and the
/// tray holds no reference to the monitor — and because "the machine is
/// pretending to be offline" is a property of the process, not of one instance.
static FORCED_OFFLINE: AtomicBool = AtomicBool::new(false);

/// Forces the monitor to report `Offline` regardless of probe results.
/// Exists so offline mode can be exercised without dropping the network.
///
/// This only decides what the *next* evaluation returns. To apply it now rather
/// than at the next tick, call [`ConnectivityMonitor::refresh_override`].
pub fn set_forced_offline(on: bool) {
    if FORCED_OFFLINE.swap(on, Ordering::SeqCst) != on {
        info!(
            "connectivity: simulated offline {}",
            if on { "engaged" } else { "cleared" }
        );
    }
}

pub fn forced_offline() -> bool {
    FORCED_OFFLINE.load(Ordering::SeqCst)
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
        Self {
            client,
            url: url.into(),
        }
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
                // The whole source chain, not just `{e}`. reqwest's outermost
                // error is always "error sending request for url (...)", which
                // says nothing — a DNS failure, a refused connection and a
                // rejected certificate all render identically. The real cause
                // is nested underneath.
                let mut detail = e.to_string();
                let mut src: Option<&(dyn std::error::Error + 'static)> =
                    std::error::Error::source(&e);
                while let Some(s) = src {
                    detail.push_str(&format!("\n  caused by: {s}"));
                    src = s.source();
                }
                warn!(
                    "connectivity probe: unreachable — {detail}\n  \
                     (timeout={}, connect={}, request={})",
                    e.is_timeout(),
                    e.is_connect(),
                    e.is_request()
                );
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

    /// Re-evaluate right after [`set_forced_offline`] was flipped.
    ///
    /// While the override is on a probe cannot change the answer, so it is
    /// skipped and `Offline` applies at once instead of up to `PROBE_TIMEOUT`
    /// later. With the override off this is a plain [`Self::poll_now`], which is
    /// what restores the real state without waiting for the next tick.
    pub async fn refresh_override(&self) -> Connectivity {
        if forced_offline() {
            return self.force_offline_now();
        }
        self.poll_now().await
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

    // Takes the lock itself. Kept separate from `refresh_override` so no
    // `MutexGuard` ever lives inside an async fn's state machine — the future
    // has to stay `Send` for the caller to spawn it.
    fn force_offline_now(&self) -> Connectivity {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.force_offline(&mut inner)
    }

    /// Report `Offline` and drop the counters on the floor.
    ///
    /// Zeroing is what stops the override from being a one-way door: failures
    /// racked up while simulating would otherwise still be on the clock when it
    /// is switched off, and the machine would sit Offline working them off
    /// instead of recovering on the first good probe.
    fn force_offline(&self, inner: &mut MonitorInner) -> Connectivity {
        inner.consecutive_failures = 0;
        inner.consecutive_successes = 0;
        self.publish(Connectivity::Offline)
    }

    // Advance the state machine with one probe result.
    // Must be called with `inner` already locked by the caller.
    fn apply_result(&self, inner: &mut MonitorInner, success: bool) -> Connectivity {
        // The override wins outright — the probe still ran (the loop must keep
        // turning so clearing the override recovers without a restart), but its
        // answer is discarded.
        if forced_offline() {
            return self.force_offline(inner);
        }

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

        self.publish(new_state)
    }

    // Store the new state and notify subscribers if it actually changed.
    fn publish(&self, new_state: Connectivity) -> Connectivity {
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
        fn with_script(results: impl IntoIterator<Item = bool>) -> (Self, Arc<AtomicUsize>) {
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

    /// The override is process-global and `cargo test` runs these in parallel
    /// threads, so without this a simulation test leaks `Offline` into whatever
    /// else happens to be probing at that moment. A tokio mutex rather than a
    /// std one because it is held across awaits.
    static OVERRIDE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Releasing exclusivity also clears the override, so a test that panicked
    /// mid-simulation cannot leave the rest of the suite pretending to be offline.
    struct Exclusive {
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Exclusive {
        fn drop(&mut self) {
            set_forced_offline(false);
        }
    }

    async fn exclusive() -> Exclusive {
        Exclusive {
            _guard: OVERRIDE_GUARD.lock().await,
        }
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
        let _exclusive = exclusive().await;
        // THE key anti-flapping test: one transient failure must not declare Offline.
        let m = monitor_from_script([false]);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Degraded);
    }

    #[tokio::test]
    async fn three_consecutive_failures_declare_offline() {
        let _exclusive = exclusive().await;
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
        let _exclusive = exclusive().await;
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
        let _exclusive = exclusive().await;
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
        let _exclusive = exclusive().await;
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
        let _exclusive = exclusive().await;
        let m = monitor_from_script([false]);
        assert_eq!(m.state(), Connectivity::Online);
        let returned = m.poll_now().await;
        assert_eq!(returned, Connectivity::Degraded);
        assert_eq!(m.state(), Connectivity::Degraded);
    }

    // -----------------------------------------------------------------------
    // Forced-offline override
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_override_reports_offline_even_when_the_probe_succeeds() {
        let _exclusive = exclusive().await;
        let m = monitor_from_script([true, true]);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Online);

        set_forced_offline(true);
        assert!(forced_offline());
        assert_eq!(m.poll_now().await, Connectivity::Offline);
        assert_eq!(m.state(), Connectivity::Offline);
    }

    #[tokio::test]
    async fn clearing_the_override_returns_online_on_the_next_success() {
        let _exclusive = exclusive().await;
        let m = monitor_from_script([true, true]);
        set_forced_offline(true);
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Offline);

        set_forced_offline(false);
        assert_eq!(m.poll_now().await, Connectivity::Online);
    }

    #[tokio::test]
    async fn failures_racked_up_while_simulating_do_not_delay_recovery() {
        // Two real failures land while the override is on. If they had been
        // counted, the first failure after clearing it would be the third
        // consecutive one and we would drop straight to Offline instead of
        // Degraded — a simulation that quietly made the next real blip worse.
        let _exclusive = exclusive().await;
        let m = monitor_from_script([false, false, false]);
        set_forced_offline(true);
        m.poll_now().await;
        m.poll_now().await;
        assert_eq!(m.state(), Connectivity::Offline);

        set_forced_offline(false);
        assert_eq!(m.poll_now().await, Connectivity::Degraded);
    }

    #[tokio::test]
    async fn the_simulated_transition_is_broadcast_exactly_once() {
        let _exclusive = exclusive().await;
        let m = monitor_from_script([true, true, true]);
        let mut rx = m.subscribe();
        assert_eq!(*rx.borrow(), Connectivity::Online);

        set_forced_offline(true);
        m.poll_now().await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Connectivity::Offline);

        // Still forced: the state has not changed, so nothing may be sent.
        m.poll_now().await;
        assert!(
            !rx.has_changed().unwrap(),
            "a repeated forced Offline must not be broadcast again"
        );
    }

    #[tokio::test]
    async fn refresh_override_applies_at_once_without_probing() {
        let _exclusive = exclusive().await;
        let (probe, count) = FakeProbe::with_script([]);
        let m = Arc::new(ConnectivityMonitor::new(probe));

        set_forced_offline(true);
        // Spawned by `lib.rs`, so the future has to be Send — asserted here
        // rather than discovered as a compile error in a file this module
        // cannot see.
        fn assert_send<T: Send>(_: &T) {}
        let fut = m.refresh_override();
        assert_send(&fut);
        assert_eq!(fut.await, Connectivity::Offline);
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a probe cannot change a forced Offline, so it must not be waited on"
        );

        set_forced_offline(false);
        assert_eq!(m.refresh_override().await, Connectivity::Online);
        assert_eq!(count.load(Ordering::SeqCst), 1, "clearing it must re-probe");
    }

    // -----------------------------------------------------------------------
    // Timing tests — tokio::time::pause + advance; zero real-time elapsed.
    // -----------------------------------------------------------------------

    /// The override must not stall the probe loop: clearing it has to recover on
    /// the next scheduled tick, with no restart and no manual poll.
    #[tokio::test(start_paused = true)]
    async fn the_probe_loop_keeps_running_while_simulating() {
        let _exclusive = exclusive().await;
        let (probe, count) = FakeProbe::with_script([]); // every probe succeeds
        let m = Arc::new(ConnectivityMonitor::new(probe));
        let m2 = m.clone();
        tokio::spawn(async move { m2.run().await });
        settle().await;

        set_forced_offline(true);
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert_eq!(m.state(), Connectivity::Offline);
        let probes = count.load(Ordering::SeqCst);
        assert!(probes >= 1, "the loop must keep probing while simulating");

        set_forced_offline(false);
        // Offline shortens the interval to 5 s, so one window is enough.
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(m.state(), Connectivity::Online);
        assert!(count.load(Ordering::SeqCst) > probes);
    }

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

#[cfg(test)]
mod live_diagnostics {
    //! Not part of the suite. Run explicitly:
    //!   cargo test --lib live_diagnostics -- --ignored --nocapture
    //!
    //! Exists because the app cannot reach api.groupme.com while every other
    //! client on the same machine can. That asymmetry points at the TLS stack,
    //! and reqwest's top-level error hides the cause.
    use super::*;

    fn chain(e: &reqwest::Error) -> String {
        let mut s = format!("{e}");
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
        while let Some(x) = src {
            s.push_str(&format!("\n    caused by: {x}"));
            src = x.source();
        }
        s
    }

    #[tokio::test]
    #[ignore]
    async fn what_exactly_is_failing() {
        let url = "https://api.groupme.com/v3/users/me";

        println!("\n=== default builder (what the app uses) ===");
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => match c.get(url).send().await {
                Ok(r) => println!("  OK  status {}", r.status()),
                Err(e) => println!("  ERR {}", chain(&e)),
            },
            Err(e) => println!("  builder failed: {e}"),
        }

        println!("\n=== plain HTTP (isolates TLS from DNS/connect) ===");
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => match c.get("http://example.com").send().await {
                Ok(r) => println!(
                    "  OK  status {}  <- network fine, so failure above is TLS",
                    r.status()
                ),
                Err(e) => println!("  ERR {}", chain(&e)),
            },
            Err(e) => println!("  builder failed: {e}"),
        }

        println!("\n=== TLS to a different host (is it GroupMe-specific?) ===");
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => match c.get("https://api.github.com").send().await {
                Ok(r) => println!("  OK  status {}", r.status()),
                Err(e) => println!("  ERR {}", chain(&e)),
            },
            Err(e) => println!("  builder failed: {e}"),
        }

        println!("\n=== ignoring cert validation (proves trust-store theory) ===");
        match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(true)
            .build()
        {
            Ok(c) => match c.get(url).send().await {
                Ok(r) => println!(
                    "  OK  status {}  <- SUCCEEDS without validation => the chain is\n      \
                     being rejected. rustls uses bundled webpki roots and ignores the\n      \
                     Windows certificate store.",
                    r.status()
                ),
                Err(e) => println!(
                    "  ERR {}  <- still fails, so NOT a trust-store issue",
                    chain(&e)
                ),
            },
            Err(e) => println!("  builder failed: {e}"),
        }
        println!();
    }
}
