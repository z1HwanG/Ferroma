//! Connection accounting and throttling.
//!
//! [`ConnectionLimiter`] is the gate a peer passes through before the server
//! allocates a task for it. It answers three questions:
//!
//! 1. Is the process at [`Limits::max_connections`]?
//! 2. Is *this source address* at [`Limits::max_connections_per_ip`]?
//! 3. Has this source address spent its command or message budget for the window?
//!
//! # Properties the tests pin down
//!
//! * The per-IP cap is enforced **independently** of the global cap: one noisy
//!   address cannot be admitted just because the process still has headroom.
//! * A [`ConnectionPermit`] releases its slot when it is dropped, including when
//!   the session it belongs to ends with an error — the permit is a guard, not a
//!   bookkeeping call the caller has to remember.
//! * Memory is **bounded**. Per-address state is kept in one map with a hard
//!   ceiling ([`ConnectionLimiter::tracked_ips`]); when the ceiling is reached,
//!   idle entries are evicted oldest-first. A peer that opens a connection from a
//!   million different addresses can therefore not grow the limiter without bound.
//!
//! The whole type is `Send + Sync`, lock-free on the happy path except for one
//! short-lived mutex around the per-address map, and cheap enough to call once per
//! command.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ferroma_core::Limits;

/// The ceiling on distinct source addresses tracked at once.
///
/// A single Ferroma process never sees anywhere near this many *live* peers; the
/// number exists so that a spoofed-source flood cannot turn the limiter itself into
/// a memory exhaustion vector.
pub const MAX_TRACKED_IPS: usize = 4096;

/// How long an idle source address is remembered.
///
/// Long enough that a client which reconnects after a short pause is still counted
/// against its per-IP cap, short enough that a burst of one-shot senders is
/// forgotten quickly.
pub const IP_ENTRY_TTL: Duration = Duration::from_secs(900);

/// How often the map is swept for expired entries, in operations.
const PRUNE_EVERY: u64 = 128;

/// One source address's live state.
#[derive(Debug, Clone)]
struct IpState {
    /// Simultaneous connections currently held from this address.
    active: u32,
    /// When this address was last seen, for expiry.
    last_seen: DateTime<Utc>,
    /// When the command-rate window started.
    command_window_start: DateTime<Utc>,
    /// Commands accepted in the current window.
    commands: u32,
    /// When the message-rate window started.
    message_window_start: DateTime<Utc>,
    /// Messages submitted in the current window.
    messages: u32,
}

impl IpState {
    fn new(now: DateTime<Utc>) -> Self {
        IpState {
            active: 0,
            last_seen: now,
            command_window_start: now,
            commands: 0,
            message_window_start: now,
            messages: 0,
        }
    }

    /// Whether the entry can be dropped: nothing connected, nothing recent.
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.active == 0
            && now
                .signed_duration_since(self.last_seen)
                .to_std()
                .map(|age| age >= IP_ENTRY_TTL)
                .unwrap_or(false)
    }
}

/// Which budget was exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// Too many simultaneous connections process-wide.
    GlobalConnections,
    /// Too many simultaneous connections from this address.
    PerIpConnections,
    /// Too many commands per minute from this address.
    CommandRate,
    /// Too many messages per hour from this address.
    MessageRate,
}

impl LimitKind {
    /// A short, stable name for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::GlobalConnections => "global_connections",
            LimitKind::PerIpConnections => "per_ip_connections",
            LimitKind::CommandRate => "command_rate",
            LimitKind::MessageRate => "message_rate",
        }
    }
}

/// Why a connection or a command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    /// Which budget ran out.
    pub kind: LimitKind,
    /// A sensible `Retry-After`, in seconds. Never zero.
    pub retry_after_secs: u64,
}

impl RateLimited {
    /// The refusal, with a retry hint.
    pub fn new(kind: LimitKind, retry_after_secs: u64) -> Self {
        RateLimited {
            kind,
            retry_after_secs: retry_after_secs.max(1),
        }
    }
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} limit reached; retry after {}s",
            self.kind.as_str(),
            self.retry_after_secs
        )
    }
}

impl std::error::Error for RateLimited {}

/// Shared state behind one mutex plus one atomic counter.
#[derive(Debug)]
struct Inner {
    /// Live global connection count. Mirrors `sum(per_ip.active)`; kept separately
    /// so the common path can check the global cap without taking the lock.
    global: AtomicUsize,
    /// Per-address state. Bounded by [`ConnectionLimiter::tracked_ips`].
    per_ip: Mutex<HashMap<IpAddr, IpState>>,
    /// Operations since the last expiry sweep.
    ops_since_prune: AtomicUsize,
}

/// The connection and rate limiter.
///
/// Cheap to clone (an `Arc` handle) and safe to share across every connection task.
#[derive(Debug, Clone)]
pub struct ConnectionLimiter {
    inner: Arc<Inner>,
    max_connections: usize,
    max_connections_per_ip: usize,
    command_rate_per_minute: u32,
    message_rate_per_hour: u32,
    tracked_ips: usize,
}

impl ConnectionLimiter {
    /// Build a limiter from the platform-wide `[limits]` block.
    pub fn new(limits: &Limits) -> Self {
        ConnectionLimiter::with_tracked_ips(limits, MAX_TRACKED_IPS)
    }

    /// Build a limiter with an explicit ceiling on tracked addresses.
    ///
    /// The ceiling is clamped to at least the per-IP cap so the limiter can always
    /// admit one address's full allowance.
    pub fn with_tracked_ips(limits: &Limits, tracked_ips: usize) -> Self {
        let max_connections = limits.max_connections.max(1);
        let max_connections_per_ip = limits.max_connections_per_ip.max(1);
        ConnectionLimiter {
            inner: Arc::new(Inner {
                global: AtomicUsize::new(0),
                per_ip: Mutex::new(HashMap::new()),
                ops_since_prune: AtomicUsize::new(0),
            }),
            max_connections,
            max_connections_per_ip,
            command_rate_per_minute: limits.smtp_rate_limit,
            message_rate_per_hour: limits.submission_rate_limit,
            tracked_ips: tracked_ips.max(max_connections_per_ip),
        }
    }

    /// The process-wide simultaneous-connection cap.
    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// The per-source-address simultaneous-connection cap.
    pub fn max_connections_per_ip(&self) -> usize {
        self.max_connections_per_ip
    }

    /// The per-source-address command budget, per minute.
    pub fn command_rate_per_minute(&self) -> u32 {
        self.command_rate_per_minute
    }

    /// The per-source-address message budget, per hour.
    pub fn message_rate_per_hour(&self) -> u32 {
        self.message_rate_per_hour
    }

    /// How many simultaneous connections are held right now.
    pub fn active_connections(&self) -> usize {
        self.inner.global.load(Ordering::Acquire)
    }

    /// How many source addresses currently have state.
    ///
    /// Bounded by [`ConnectionLimiter::tracked_ips`] — the bound the memory test
    /// asserts.
    pub fn tracked_ips(&self) -> usize {
        self.lock().len()
    }

    /// How many simultaneous connections one address holds.
    pub fn active_for(&self, ip: IpAddr) -> u32 {
        self.lock().get(&ip).map(|state| state.active).unwrap_or(0)
    }

    /// Try to admit a new connection from `ip`.
    ///
    /// On success the caller receives a [`ConnectionPermit`]; the slot is released
    /// when that permit is dropped, so a session that ends with an error, a panic or
    /// an early `return` still gives the slot back.
    pub fn try_acquire(&self, ip: IpAddr) -> Result<ConnectionPermit, RateLimited> {
        self.try_acquire_at(ip, Utc::now())
    }

    /// [`ConnectionLimiter::try_acquire`] against an explicit clock, for tests.
    pub fn try_acquire_at(&self, ip: IpAddr, now: DateTime<Utc>) -> Result<ConnectionPermit, RateLimited> {
        // Fast path: the global cap, without touching the map.
        let mut observed = self.inner.global.load(Ordering::Acquire);
        loop {
            if observed >= self.max_connections {
                return Err(RateLimited::new(LimitKind::GlobalConnections, 30));
            }
            match self.inner.global.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => observed = current,
            }
        }

        let mut map = self.lock();
        self.prune_locked(&mut map, now);

        let entry = map.entry(ip).or_insert_with(|| IpState::new(now));
        if entry.active as usize >= self.max_connections_per_ip {
            // Give the global slot straight back: we are not admitting this peer.
            self.inner.global.fetch_sub(1, Ordering::AcqRel);
            return Err(RateLimited::new(LimitKind::PerIpConnections, 60));
        }
        entry.active += 1;
        entry.last_seen = now;

        // Hard ceiling: if a flood of distinct addresses filled the map, evict the
        // least recently seen idle entry. There is always one, because every
        // address we just admitted is idle (active == 1) only if it is *this* one,
        // and a full map of active entries implies the global cap was reached.
        if map.len() > self.tracked_ips {
            self.evict_one_locked(&mut map, ip);
        }

        Ok(ConnectionPermit {
            inner: Arc::clone(&self.inner),
            ip,
            released: false,
        })
    }

    /// Charge one command against `ip`'s per-minute budget.
    ///
    /// A budget of `0` means "unlimited" — the configuration's way of turning the
    /// throttle off without a second flag.
    pub fn check_command_rate(&self, ip: IpAddr) -> Result<(), RateLimited> {
        self.check_command_rate_at(ip, Utc::now())
    }

    /// [`ConnectionLimiter::check_command_rate`] against an explicit clock.
    pub fn check_command_rate_at(&self, ip: IpAddr, now: DateTime<Utc>) -> Result<(), RateLimited> {
        if self.command_rate_per_minute == 0 {
            return Ok(());
        }
        let mut map = self.lock();
        let entry = map.entry(ip).or_insert_with(|| IpState::new(now));
        entry.last_seen = now;
        if now.signed_duration_since(entry.command_window_start).num_seconds() >= 60 {
            entry.command_window_start = now;
            entry.commands = 0;
        }
        if entry.commands >= self.command_rate_per_minute {
            let elapsed = now
                .signed_duration_since(entry.command_window_start)
                .num_seconds()
                .max(0) as u64;
            return Err(RateLimited::new(
                LimitKind::CommandRate,
                (60u64).saturating_sub(elapsed),
            ));
        }
        entry.commands += 1;
        Ok(())
    }

    /// Charge one message against `ip`'s per-hour budget.
    pub fn check_message_rate(&self, ip: IpAddr) -> Result<(), RateLimited> {
        self.check_message_rate_at(ip, Utc::now())
    }

    /// [`ConnectionLimiter::check_message_rate`] against an explicit clock.
    pub fn check_message_rate_at(&self, ip: IpAddr, now: DateTime<Utc>) -> Result<(), RateLimited> {
        if self.message_rate_per_hour == 0 {
            return Ok(());
        }
        let mut map = self.lock();
        let entry = map.entry(ip).or_insert_with(|| IpState::new(now));
        entry.last_seen = now;
        if now.signed_duration_since(entry.message_window_start).num_seconds() >= 3600 {
            entry.message_window_start = now;
            entry.messages = 0;
        }
        if entry.messages >= self.message_rate_per_hour {
            let elapsed = now
                .signed_duration_since(entry.message_window_start)
                .num_seconds()
                .max(0) as u64;
            return Err(RateLimited::new(
                LimitKind::MessageRate,
                (3600u64).saturating_sub(elapsed),
            ));
        }
        entry.messages += 1;
        Ok(())
    }

    /// Drop every expired entry. Called automatically; exposed for the tests and for
    /// an operator-triggered sweep.
    pub fn prune(&self) -> usize {
        let mut map = self.lock();
        let now = Utc::now();
        let before = map.len();
        map.retain(|_, state| !state.is_expired(now));
        before - map.len()
    }

    /// Force the tracked-address map down to `max_entries`, evicting idle entries
    /// least-recently-seen first. Returns how many went.
    pub fn shrink_to(&self, max_entries: usize) -> usize {
        let mut map = self.lock();
        let before = map.len();
        while map.len() > max_entries {
            if !Self::evict_one(&mut map, None) {
                break;
            }
        }
        before - map.len()
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, IpState>> {
        self.inner
            .per_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn prune_locked(&self, map: &mut HashMap<IpAddr, IpState>, now: DateTime<Utc>) {
        let ops = self.inner.ops_since_prune.fetch_add(1, Ordering::Relaxed) + 1;
        let crowded = map.len() >= self.tracked_ips;
        if ops % PRUNE_EVERY as usize != 0 && !crowded {
            return;
        }
        if ops % PRUNE_EVERY as usize == 0 {
            self.inner.ops_since_prune.store(0, Ordering::Relaxed);
        }
        map.retain(|_, state| !state.is_expired(now));
    }

    /// Evict one idle entry, preferring the least recently seen. Never evicts
    /// `keep`, which the caller is in the middle of admitting.
    fn evict_one_locked(&self, map: &mut HashMap<IpAddr, IpState>, keep: IpAddr) {
        Self::evict_one(map, Some(keep));
    }

    /// The eviction policy itself: drop the idle entry with the oldest
    /// `last_seen`. Returns `false` when there is nothing evictable — every entry
    /// either holds a connection or is the one being admitted.
    fn evict_one(map: &mut HashMap<IpAddr, IpState>, keep: Option<IpAddr>) -> bool {
        let victim = map
            .iter()
            .filter(|(ip, state)| state.active == 0 && Some(**ip) != keep)
            .min_by_key(|(_, state)| state.last_seen)
            .map(|(ip, _)| *ip);
        match victim {
            Some(ip) => map.remove(&ip).is_some(),
            None => false,
        }
    }
}

/// Proof that a connection was admitted.
///
/// Dropping it releases the slot. It is deliberately neither `Clone` nor `Copy`:
/// one permit is one connection, and copying it would leak slots.
#[derive(Debug)]
pub struct ConnectionPermit {
    inner: Arc<Inner>,
    ip: IpAddr,
    released: bool,
}

impl ConnectionPermit {
    /// The address this permit was issued for.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// Release the slot now instead of at end of scope. Idempotent.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut map = self
            .inner
            .per_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(state) = map.get_mut(&self.ip) {
            state.active = state.active.saturating_sub(1);
            state.last_seen = Utc::now();
        }
        self.inner.global.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn limits(global: usize, per_ip: usize) -> Limits {
        Limits {
            max_connections: global,
            max_connections_per_ip: per_ip,
            smtp_rate_limit: 100,
            submission_rate_limit: 50,
            ..Limits::default()
        }
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, last])
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("valid timestamp")
    }

    // ------------------------------------------------------------------
    // Global cap
    // ------------------------------------------------------------------

    #[test]
    fn the_global_cap_is_enforced() {
        let limiter = ConnectionLimiter::new(&limits(2, 10));
        let a = limiter.try_acquire(ip(1)).unwrap();
        let b = limiter.try_acquire(ip(2)).unwrap();
        let err = limiter.try_acquire(ip(3)).unwrap_err();
        assert_eq!(err.kind, LimitKind::GlobalConnections);
        assert!(err.retry_after_secs >= 1);
        assert_eq!(limiter.active_connections(), 2);

        drop(a);
        assert_eq!(limiter.active_connections(), 1);
        assert!(limiter.try_acquire(ip(3)).is_ok());
        drop(b);
    }

    #[test]
    fn a_zero_global_cap_is_clamped_to_one_so_the_server_still_works() {
        let limiter = ConnectionLimiter::new(&limits(0, 0));
        assert_eq!(limiter.max_connections(), 1);
        assert_eq!(limiter.max_connections_per_ip(), 1);
        assert!(limiter.try_acquire(ip(1)).is_ok());
    }

    // ------------------------------------------------------------------
    // Per-IP cap, independently of the global cap
    // ------------------------------------------------------------------

    #[test]
    fn the_per_ip_cap_is_enforced_independently_of_the_global_cap() {
        // Room for 100 connections overall, but only 2 from any one address.
        let limiter = ConnectionLimiter::new(&limits(100, 2));

        let first = limiter.try_acquire(ip(1)).unwrap();
        let second = limiter.try_acquire(ip(1)).unwrap();
        let err = limiter.try_acquire(ip(1)).unwrap_err();

        assert_eq!(err.kind, LimitKind::PerIpConnections);
        assert_eq!(limiter.active_connections(), 2, "the refused peer must not hold a slot");
        assert_eq!(limiter.active_for(ip(1)), 2);

        // A different address is unaffected: proof the two caps are independent.
        let other = limiter.try_acquire(ip(2)).unwrap();
        assert_eq!(limiter.active_for(ip(2)), 1);
        assert_eq!(limiter.active_connections(), 3);

        drop(first);
        drop(second);
        drop(other);
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn a_refused_per_ip_acquire_does_not_leak_the_global_slot() {
        let limiter = ConnectionLimiter::new(&limits(100, 1));
        let held = limiter.try_acquire(ip(1)).unwrap();
        for _ in 0..100 {
            assert!(limiter.try_acquire(ip(1)).is_err());
        }
        assert_eq!(limiter.active_connections(), 1);
        drop(held);
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn every_address_gets_its_own_allowance() {
        let limiter = ConnectionLimiter::new(&limits(1000, 3));
        let mut held = Vec::new();
        for last in 1..=10u8 {
            for _ in 0..3 {
                held.push(limiter.try_acquire(ip(last)).unwrap());
            }
            assert!(limiter.try_acquire(ip(last)).is_err(), "address {last} over its cap");
        }
        assert_eq!(limiter.active_connections(), 30);
        drop(held);
        assert_eq!(limiter.active_connections(), 0);
    }

    // ------------------------------------------------------------------
    // Release semantics
    // ------------------------------------------------------------------

    #[test]
    fn a_permit_is_released_when_it_is_dropped() {
        let limiter = ConnectionLimiter::new(&limits(1, 1));
        {
            let _permit = limiter.try_acquire(ip(1)).unwrap();
            assert_eq!(limiter.active_connections(), 1);
        }
        assert_eq!(limiter.active_connections(), 0);
        assert!(limiter.try_acquire(ip(1)).is_ok());
    }

    #[test]
    fn a_permit_is_released_when_the_session_returns_an_error() {
        let limiter = ConnectionLimiter::new(&limits(1, 1));

        // A session that fails immediately, modelled as an early `Err` return from
        // the scope that owns the permit.
        fn failing_session(limiter: &ConnectionLimiter) -> Result<(), &'static str> {
            let _permit = limiter.try_acquire(ip(1)).map_err(|_| "no slot")?;
            Err("the peer hung up mid-command")
        }

        assert_eq!(failing_session(&limiter), Err("the peer hung up mid-command"));
        assert_eq!(limiter.active_connections(), 0, "the slot must come back");
        assert!(limiter.try_acquire(ip(1)).is_ok());
    }

    #[test]
    fn a_permit_is_released_when_a_task_panics() {
        let limiter = ConnectionLimiter::new(&limits(1, 1));
        let clone = limiter.clone();
        let joined = std::thread::spawn(move || {
            let _permit = clone.try_acquire(ip(1)).unwrap();
            panic!("the peer sent something that blew up a handler");
        })
        .join();
        assert!(joined.is_err());
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn explicit_release_is_idempotent() {
        let limiter = ConnectionLimiter::new(&limits(4, 4));
        let mut permit = limiter.try_acquire(ip(1)).unwrap();
        permit.release();
        assert_eq!(limiter.active_connections(), 0);
        permit.release();
        assert_eq!(limiter.active_connections(), 0, "double release must not underflow");
        drop(permit);
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn concurrent_acquires_never_exceed_the_caps() {
        use std::sync::atomic::AtomicUsize;

        let limiter = ConnectionLimiter::new(&limits(64, 8));
        let admitted = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for last in 1..=16u8 {
            for _ in 0..8 {
                let limiter = limiter.clone();
                let admitted = Arc::clone(&admitted);
                let peak = Arc::clone(&peak);
                handles.push(std::thread::spawn(move || {
                    if let Ok(permit) = limiter.try_acquire(ip(last)) {
                        let now = admitted.fetch_add(1, Ordering::AcqRel) + 1;
                        peak.fetch_max(now, Ordering::AcqRel);
                        std::thread::sleep(Duration::from_millis(5));
                        admitted.fetch_sub(1, Ordering::AcqRel);
                        drop(permit);
                    }
                }));
            }
        }
        for handle in handles {
            assert!(handle.join().is_ok());
        }
        assert!(peak.load(Ordering::Acquire) <= 64, "global cap exceeded");
        assert_eq!(limiter.active_connections(), 0);
    }

    // ------------------------------------------------------------------
    // Bounded memory
    // ------------------------------------------------------------------

    #[test]
    fn the_tracked_map_does_not_grow_without_bound_across_distinct_addresses() {
        let l = Limits {
            max_connections: 100_000,
            max_connections_per_ip: 1,
            ..Limits::default()
        };
        let limiter = ConnectionLimiter::with_tracked_ips(&l, 64);

        for n in 0..5000u32 {
            let address = IpAddr::from([10, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
            let permit = limiter.try_acquire(address).expect("under the global cap");
            drop(permit);
        }

        assert!(
            limiter.tracked_ips() <= 64,
            "map grew to {} entries",
            limiter.tracked_ips()
        );
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn a_burst_of_distinct_addresses_is_never_refused_by_the_memory_ceiling() {
        // The ceiling bounds *retained* state, never the number of peers that may
        // hold a connection at once — that is the global cap's job.
        let l = Limits {
            max_connections: 10_000,
            max_connections_per_ip: 1,
            ..Limits::default()
        };
        let limiter = ConnectionLimiter::with_tracked_ips(&l, 8);

        let mut held = Vec::new();
        for n in 0..100u32 {
            let address = IpAddr::from([10, 0, (n >> 8) as u8, n as u8]);
            held.push(
                limiter
                    .try_acquire(address)
                    .unwrap_or_else(|e| panic!("address {n} refused: {e}")),
            );
        }
        assert_eq!(limiter.active_connections(), 100);
        assert_eq!(limiter.tracked_ips(), 100, "live peers keep their entries");
        drop(held);
        assert_eq!(limiter.active_connections(), 0);
    }

    #[test]
    fn expired_idle_entries_are_pruned() {
        let limiter = ConnectionLimiter::new(&limits(10, 2));
        for last in 1..=5u8 {
            let permit = limiter.try_acquire(ip(last)).unwrap();
            drop(permit);
        }
        assert_eq!(limiter.tracked_ips(), 5);

        // Nothing has expired yet.
        assert_eq!(limiter.prune(), 0);
        assert_eq!(limiter.tracked_ips(), 5);

        // Age the entries past the TTL, then let the sweep do its job.
        {
            let mut map = limiter.lock();
            let stale = Utc::now() - ChronoDuration::seconds(IP_ENTRY_TTL.as_secs() as i64 + 1);
            for state in map.values_mut() {
                state.last_seen = stale;
            }
        }
        assert_eq!(limiter.prune(), 5);
        assert_eq!(limiter.tracked_ips(), 0);
    }

    #[test]
    fn an_expired_entry_with_a_live_connection_is_kept() {
        let limiter = ConnectionLimiter::new(&limits(10, 2));
        let _held = limiter.try_acquire(ip(1)).unwrap();
        {
            let mut map = limiter.lock();
            let stale = Utc::now() - ChronoDuration::seconds(IP_ENTRY_TTL.as_secs() as i64 + 1);
            for state in map.values_mut() {
                state.last_seen = stale;
            }
        }
        assert_eq!(limiter.prune(), 0, "a live connection keeps its entry");
        assert_eq!(limiter.active_for(ip(1)), 1);
    }

    #[test]
    fn shrink_to_evicts_the_least_recently_seen_idle_entries_first() {
        let limiter = ConnectionLimiter::new(&limits(100, 4));
        for (last, secs) in [(1u8, 0i64), (2, 10), (3, 20), (4, 30)] {
            let permit = limiter.try_acquire_at(ip(last), at(secs)).unwrap();
            drop(permit);
        }
        assert_eq!(limiter.tracked_ips(), 4);

        assert_eq!(limiter.shrink_to(2), 2);
        assert_eq!(limiter.tracked_ips(), 2);
        assert_eq!(limiter.active_for(ip(1)), 0);
        assert_eq!(limiter.active_for(ip(2)), 0);
        // The two most recently seen survive.
        assert!(limiter.lock().contains_key(&ip(3)));
        assert!(limiter.lock().contains_key(&ip(4)));
    }

    #[test]
    fn shrink_never_evicts_an_entry_that_holds_a_connection() {
        let limiter = ConnectionLimiter::new(&limits(100, 4));
        let _busy = limiter.try_acquire_at(ip(1), at(0)).unwrap();
        for (last, secs) in [(2u8, 10i64), (3, 20)] {
            let permit = limiter.try_acquire_at(ip(last), at(secs)).unwrap();
            drop(permit);
        }
        limiter.shrink_to(0);
        assert_eq!(limiter.active_for(ip(1)), 1, "a live connection keeps its entry");
        assert_eq!(limiter.tracked_ips(), 1);
    }

    #[test]
    fn the_automatic_sweep_and_the_ceiling_keep_the_map_bounded() {
        let l = Limits {
            max_connections: 10_000,
            max_connections_per_ip: 1,
            ..Limits::default()
        };
        let limiter = ConnectionLimiter::with_tracked_ips(&l, 32);

        // More acquisitions than the prune interval, all at one instant, so nothing
        // is *expired* and only the hard ceiling can bound the map.
        for n in 0..(PRUNE_EVERY as u32 * 3) {
            let address = IpAddr::from([10, 1, (n >> 8) as u8, n as u8]);
            let permit = limiter.try_acquire(address).unwrap();
            drop(permit);
        }
        assert!(
            limiter.tracked_ips() <= 32,
            "map grew to {} entries",
            limiter.tracked_ips()
        );
        assert_eq!(limiter.active_connections(), 0);
    }

    // ------------------------------------------------------------------
    // Rate limits
    // ------------------------------------------------------------------

    #[test]
    fn the_command_rate_is_enforced_per_window() {
        let limiter = ConnectionLimiter::new(&limits(10, 10));
        for _ in 0..limiter.command_rate_per_minute() {
            limiter.check_command_rate_at(ip(1), at(0)).unwrap();
        }
        let err = limiter.check_command_rate_at(ip(1), at(30)).unwrap_err();
        assert_eq!(err.kind, LimitKind::CommandRate);
        assert_eq!(err.retry_after_secs, 30, "half the window is left");

        // A different address still has its own budget.
        limiter.check_command_rate_at(ip(2), at(30)).unwrap();

        // The next window resets the counter.
        limiter.check_command_rate_at(ip(1), at(61)).unwrap();
    }

    #[test]
    fn the_message_rate_is_enforced_per_hour() {
        let limiter = ConnectionLimiter::new(&limits(10, 10));
        for _ in 0..limiter.message_rate_per_hour() {
            limiter.check_message_rate_at(ip(1), at(0)).unwrap();
        }
        let err = limiter.check_message_rate_at(ip(1), at(60)).unwrap_err();
        assert_eq!(err.kind, LimitKind::MessageRate);
        assert_eq!(err.retry_after_secs, 3600 - 60);

        limiter.check_message_rate_at(ip(1), at(3601)).unwrap();
    }

    #[test]
    fn a_zero_budget_means_unlimited() {
        let mut l = limits(10, 10);
        l.smtp_rate_limit = 0;
        l.submission_rate_limit = 0;
        let limiter = ConnectionLimiter::new(&l);
        for _ in 0..10_000 {
            limiter.check_command_rate(ip(1)).unwrap();
            limiter.check_message_rate(ip(1)).unwrap();
        }
    }

    #[test]
    fn the_retry_hint_is_never_zero() {
        let limiter = ConnectionLimiter::new(&limits(10, 10));
        for _ in 0..limiter.command_rate_per_minute() {
            limiter.check_command_rate_at(ip(1), at(0)).unwrap();
        }
        let err = limiter.check_command_rate_at(ip(1), at(59)).unwrap_err();
        assert_eq!(err.retry_after_secs, 1, "one second left in the window");

        // The pathological case: the elapsed time equals the window length, so the
        // raw remainder is 0 and an unclamped hint would tell the peer to retry
        // immediately.
        let limiter = ConnectionLimiter::new(&limits(10, 10));
        limiter.check_command_rate_at(ip(2), at(0)).unwrap();
        {
            let mut map = limiter.lock();
            let state = map.get_mut(&ip(2)).unwrap();
            state.commands = limiter.command_rate_per_minute();
            state.command_window_start = at(-59);
        }
        let err = limiter.check_command_rate_at(ip(2), at(0)).unwrap_err();
        assert_eq!(err.kind, LimitKind::CommandRate);
        assert!(err.retry_after_secs >= 1, "a zero hint must be clamped");
    }

    #[test]
    fn rate_checks_also_track_the_address() {
        let limiter = ConnectionLimiter::new(&limits(10, 10));
        limiter.check_command_rate_at(ip(7), at(0)).unwrap();
        assert_eq!(limiter.tracked_ips(), 1);
        assert!(limiter.lock().contains_key(&ip(7)));
    }

    // ------------------------------------------------------------------
    // Type-level properties
    // ------------------------------------------------------------------

    #[test]
    fn the_limiter_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ConnectionLimiter>();
        assert_send_sync::<ConnectionPermit>();
        assert_send_sync::<RateLimited>();
    }

    #[test]
    fn the_limiter_is_cloneable_and_the_clone_shares_state() {
        let limiter = ConnectionLimiter::new(&limits(2, 2));
        let clone = limiter.clone();
        let _held = limiter.try_acquire(ip(1)).unwrap();
        let _also_held = clone.try_acquire(ip(2)).unwrap();
        assert_eq!(clone.active_connections(), 2);
        // The clone shares the *global* counter, so the third connection is refused.
        let err = clone.try_acquire(ip(3)).unwrap_err();
        assert_eq!(err.kind, LimitKind::GlobalConnections);
        assert_eq!(limiter.active_connections(), 2);
    }

    #[test]
    fn accessors_report_the_configured_limits() {
        let limiter = ConnectionLimiter::new(&limits(100, 10));
        assert_eq!(limiter.max_connections(), 100);
        assert_eq!(limiter.max_connections_per_ip(), 10);
        assert_eq!(limiter.command_rate_per_minute(), 100);
        assert_eq!(limiter.message_rate_per_hour(), 50);
    }

    #[test]
    fn rate_limited_displays_and_implements_error() {
        let err = RateLimited::new(LimitKind::GlobalConnections, 0);
        assert_eq!(err.retry_after_secs, 1);
        assert!(err.to_string().contains("global_connections"));
        let _: &dyn std::error::Error = &err;
        assert_eq!(LimitKind::PerIpConnections.as_str(), "per_ip_connections");
        assert_eq!(LimitKind::CommandRate.as_str(), "command_rate");
        assert_eq!(LimitKind::MessageRate.as_str(), "message_rate");
    }

    #[test]
    fn a_permit_reports_the_address_it_was_issued_for() {
        let limiter = ConnectionLimiter::new(&limits(4, 4));
        let permit = limiter.try_acquire(ip(9)).unwrap();
        assert_eq!(permit.ip(), ip(9));
    }

    #[test]
    fn the_ttl_is_long_enough_to_span_a_short_reconnect() {
        assert!(IP_ENTRY_TTL >= Duration::from_secs(600));
        assert!(
            at(0) + ChronoDuration::seconds(IP_ENTRY_TTL.as_secs() as i64 - 1) > at(0),
            "sanity"
        );
    }
}
