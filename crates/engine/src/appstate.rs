//! Follow-up of incomplete app-state (syncd) syncs.
//!
//! The library reports a batched sync that left collections behind through
//! `Event::AppStateSyncFailed`: `skipped` means another writer held the
//! collection and nothing was fetched for it, `retryable` that a later attempt
//! can work (e.g. a decode key the phone has not re-shared yet). Pins, archive
//! and mute flags of the phone live in those collections, so until they sync
//! the chat list shows none of them. The library keeps its bootstrap armed for
//! the next connection, but a session can run for hours; this schedules a few
//! full resyncs of the affected collections meanwhile.
//!
//! A full resync starts from a reset collection. The library asks the server for
//! a snapshot, but discards one that is not newer than the persisted version
//! ("Skipping stale snapshot") and then still applies the patches that follow
//! that snapshot, which start at or below the persisted version: every attempt
//! ends in "patch version mismatch" and the library retries it forever
//! (wacore `appstate_sync.rs` `process_patch_list`, wacore-appstate
//! `processor.rs` `process_patch`). Resetting the persisted version to 0 first
//! makes the snapshot apply, which rebuilds the collection from the server.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::prelude::Client;
use whatsapp_rust::sync_task::MajorSyncTask;
use whatsapp_rust::wacore::appstate::hash::HashState;
use whatsapp_rust::wacore::appstate::patch_decode::WAPatchName;
use whatsapp_rust::wacore::types::events::AppStateSyncFailed;

use crate::events::Shared;

/// Wait before a resync, so the phone has time to answer a key request and a
/// holder of the collection can finish.
const RESYNC_DELAY: Duration = Duration::from_secs(30);
/// Resyncs per generation; each is triggered by a reported failure.
pub(crate) const MAX_RESYNCS: u32 = 4;

/// Collections worth another full sync: whatever was skipped or is retryable
/// (a fatal collection is refused by the server, repeating changes nothing).
pub(crate) fn resync_targets(f: &AppStateSyncFailed) -> Vec<WAPatchName> {
    parse_names(f.skipped.iter().chain(&f.retryable))
}

/// Collections that may be reset before their resync: only the retryable ones.
/// A skipped collection was held by another writer, which may be mid-write.
pub(crate) fn reset_targets(f: &AppStateSyncFailed) -> Vec<WAPatchName> {
    parse_names(f.retryable.iter())
}

fn parse_names<'a>(names: impl Iterator<Item = &'a String>) -> Vec<WAPatchName> {
    let mut out: Vec<WAPatchName> = Vec::new();
    for name in names {
        if let Ok(parsed) = name.parse::<WAPatchName>()
            && parsed != WAPatchName::Unknown
            && !out.contains(&parsed)
        {
            out.push(parsed);
        }
    }
    out
}

/// Failures of one collection within [`FAILURE_WINDOW`] that trigger a recovery.
const FAILURE_THRESHOLD: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(2 * 60);
/// A collection is recovered at most this often.
const RECOVERY_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct CollectionHealth {
    failures: Vec<Instant>,
    last_recovery: Option<Instant>,
}

/// Decides when repeated failures of a collection justify a recovery that is
/// not covered by the per-generation resync budget.
#[derive(Default)]
pub(crate) struct RecoveryThrottle {
    collections: Vec<(String, CollectionHealth)>,
}

impl RecoveryThrottle {
    /// Records a failure of `name` at `now`; `true` when a recovery is due:
    /// [`FAILURE_THRESHOLD`] failures within [`FAILURE_WINDOW`] and none
    /// recovered during [`RECOVERY_INTERVAL`]. The caller reports a recovery
    /// that was actually scheduled with [`RecoveryThrottle::mark_recovered`];
    /// until then a due recovery stays due.
    pub(crate) fn record_failure(&mut self, name: &str, now: Instant) -> bool {
        let idx = match self.collections.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                self.collections.push((name.to_string(), CollectionHealth::default()));
                self.collections.len() - 1
            }
        };
        let health = &mut self.collections[idx].1;
        health.failures.retain(|t| now.saturating_duration_since(*t) <= FAILURE_WINDOW);
        health.failures.push(now);
        let cooled_down = health
            .last_recovery
            .is_none_or(|t| now.saturating_duration_since(t) >= RECOVERY_INTERVAL);
        health.failures.len() >= FAILURE_THRESHOLD && cooled_down
    }

    /// Starts the cool-down of `name`: a recovery was scheduled at `now`.
    pub(crate) fn mark_recovered(&mut self, name: &str, now: Instant) {
        if let Some((_, health)) = self.collections.iter_mut().find(|(n, _)| n == name) {
            health.failures.clear();
            health.last_recovery = Some(now);
        }
    }
}

/// Process-wide like [`PENDING`]: failures and recoveries span generations.
static THROTTLE: Mutex<Option<RecoveryThrottle>> = Mutex::new(None);

fn record_failures(names: &[WAPatchName], now: Instant) -> Vec<WAPatchName> {
    let mut guard = THROTTLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let throttle = guard.get_or_insert_with(RecoveryThrottle::default);
    names.iter().copied().filter(|n| throttle.record_failure(n.as_str(), now)).collect()
}

fn mark_recovered(names: &[WAPatchName], now: Instant) {
    let mut guard = THROTTLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let throttle = guard.get_or_insert_with(RecoveryThrottle::default);
    for n in names {
        throttle.mark_recovered(n.as_str(), now);
    }
}

/// Marks the collections that are not pending yet as pending and returns them.
fn claim_pending(pending: &mut Vec<String>, names: Vec<WAPatchName>) -> Vec<WAPatchName> {
    let fresh: Vec<WAPatchName> =
        names.into_iter().filter(|t| !pending.iter().any(|p| p == t.as_str())).collect();
    pending.extend(fresh.iter().map(|t| t.as_str().to_string()));
    fresh
}

/// Drops the persisted version and mutation MACs of a collection so the next
/// full sync applies the server's snapshot (see the module comment).
async fn reset_collection(client: &Client, name: WAPatchName) -> anyhow::Result<()> {
    let backend = client.persistence_manager().backend();
    // Version first: with version 0 the library wipes leftover MACs itself, so
    // a failure of the second step cannot leave MACs under a live version.
    backend.set_version(name.as_str(), HashState::default()).await?;
    backend.clear_mutation_macs(name.as_str()).await?;
    Ok(())
}

const IDLE_POLLS: u32 = 12;
const IDLE_POLL: Duration = Duration::from_secs(5);

/// Waits until no app-state chat action is in flight; `false` when still busy
/// after `polls` checks.
async fn wait_until_idle(inflight: &AtomicU32, polls: u32, every: Duration) -> bool {
    for _ in 0..polls {
        if inflight.load(Ordering::SeqCst) == 0 {
            return true;
        }
        tokio::time::sleep(every).await;
    }
    inflight.load(Ordering::SeqCst) == 0
}

/// Counts a resync against the per-generation budget; `false` when used up.
pub(crate) fn take_resync(used: &AtomicU32) -> bool {
    used.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < MAX_RESYNCS).then_some(n + 1))
        .is_ok()
}

/// Gives back one unit of the resync budget (a deferred resync did not run).
pub(crate) fn refund_resync(used: &AtomicU32) {
    // Err means the counter is already 0, so there is nothing to give back.
    let _ = used.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
}

/// Collections that are scheduled or syncing. Process-wide on purpose: an entry
/// is removed by [`PendingGuard`], also when a generation aborts its task.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn lock_pending() -> std::sync::MutexGuard<'static, Vec<String>> {
    PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Picks the targets not already in `pending`, marks them pending and spends
/// one unit of budget. Nothing is spent (and nothing marked) when every target
/// is already pending or the budget is used up, so a burst of identical
/// failure events costs one resync.
pub(crate) fn plan_resync(
    pending: &mut Vec<String>,
    used: &AtomicU32,
    targets: Vec<WAPatchName>,
) -> Vec<WAPatchName> {
    let fresh: Vec<WAPatchName> =
        targets.into_iter().filter(|t| !pending.iter().any(|p| p == t.as_str())).collect();
    if fresh.is_empty() || !take_resync(used) {
        return Vec::new();
    }
    pending.extend(fresh.iter().map(|t| t.as_str().to_string()));
    fresh
}

/// Removes its collections from [`PENDING`] when dropped (task finished or aborted).
struct PendingGuard(Vec<String>);

impl PendingGuard {
    fn finish(&mut self, name: &str) {
        self.0.retain(|n| n != name);
        lock_pending().retain(|n| n != name);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = lock_pending();
        pending.retain(|n| !self.0.contains(n));
    }
}

/// Handles `Event::AppStateSyncFailed`: logs it and schedules a full resync of
/// the collections that can still be fixed and are not already queued.
pub(crate) fn on_sync_failed(sh: &Shared, client: &Arc<Client>, f: &AppStateSyncFailed) {
    log::warn!(
        "app state sync incomplete: fatal={:?} retryable={:?} skipped={:?} connected={}",
        f.fatal,
        f.retryable,
        f.skipped,
        f.connected
    );
    let targets = resync_targets(f);
    if targets.is_empty() {
        return;
    }
    let recoveries = record_failures(&targets, Instant::now());
    let batch = {
        let mut pending = lock_pending();
        let mut batch = plan_resync(&mut pending, &sh.appstate_resyncs, targets);
        // Repeated failures recover beyond the budget, throttled per collection.
        let forced = claim_pending(&mut pending, recoveries);
        for name in &forced {
            log::warn!("app state recovery: repeated failures of {}", name.as_str());
        }
        let budgeted = batch.clone();
        batch.extend(forced);
        (batch, budgeted)
    };
    let (batch, budgeted) = batch;
    if batch.is_empty() {
        log::info!("app state resync skipped: already pending or budget used up");
        return;
    }
    let mut guard = PendingGuard(batch.iter().map(|n| n.as_str().to_string()).collect());
    let client = client.clone();
    let resettable = reset_targets(f);
    let resyncs = sh.appstate_resyncs.clone();
    let account = sh.account.clone();
    sh.tasks.spawn(async move {
        // The budget unit of the plan is refunded at most once, however many
        // of its collections time out.
        let mut refundable = !budgeted.is_empty();
        tokio::time::sleep(RESYNC_DELAY).await;
        for name in batch {
            let key = name.as_str().to_string();
            if resettable.contains(&name) {
                // A reset under a running chat action would let the phone's
                // answer to it be applied to a collection that restarts from 0.
                if !wait_until_idle(&account.app_state_inflight, IDLE_POLLS, IDLE_POLL).await {
                    log::info!("app state reset of {key} deferred: chat action in flight");
                    if refundable && budgeted.contains(&name) {
                        refundable = false;
                        refund_resync(&resyncs);
                    }
                    guard.finish(&key);
                    continue;
                }
                // Without the reset a stale persisted version makes the resync
                // fail in a loop, so a collection that cannot be reset is left alone.
                if let Err(e) = reset_collection(&client, name).await {
                    log::error!("app state reset of {key} failed: {e:#}");
                    guard.finish(&key);
                    continue;
                }
            }
            // Only now is the collection really being recovered; starting the
            // cool-down earlier would suppress retries after a failed reset.
            mark_recovered(&[name], Instant::now());
            log::info!("app state resync of {key}");
            client
                .process_sync_task(MajorSyncTask::AppStateSync { name, full_sync: true })
                .await;
            guard.finish(&key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(fatal: &[&str], retryable: &[&str], skipped: &[&str]) -> AppStateSyncFailed {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        AppStateSyncFailed::builder()
            .fatal(v(fatal))
            .retryable(v(retryable))
            .skipped(v(skipped))
            .connected(true)
            .build()
    }

    #[test]
    fn refunding_restores_the_budget_and_never_underflows() {
        let used = AtomicU32::new(0);
        refund_resync(&used);
        assert_eq!(used.load(Ordering::SeqCst), 0);
        assert!(take_resync(&used));
        refund_resync(&used);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn skipped_and_retryable_collections_are_resynced_once_each() {
        let f = failed(&["critical_block"], &["regular_low", "regular"], &["regular", "bogus", "unknown"]);
        let names: Vec<&str> = resync_targets(&f).iter().map(WAPatchName::as_str).collect();
        assert_eq!(names, vec!["regular", "regular_low"]);
        assert!(resync_targets(&failed(&["regular"], &[], &[])).is_empty());
    }

    #[test]
    fn only_retryable_collections_are_reset() {
        let f = failed(&["critical_block"], &["regular_low"], &["regular", "regular_low"]);
        let names: Vec<&str> = reset_targets(&f).iter().map(WAPatchName::as_str).collect();
        assert_eq!(names, vec!["regular_low"]);
        assert!(reset_targets(&failed(&[], &[], &["regular"])).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_reset_waits_for_chat_actions_and_gives_up_when_they_stay() {
        let busy = AtomicU32::new(1);
        assert!(!wait_until_idle(&busy, 3, Duration::from_secs(1)).await);
        let idle = AtomicU32::new(0);
        assert!(wait_until_idle(&idle, 3, Duration::from_secs(1)).await);
    }

    #[test]
    fn a_recovery_that_was_not_scheduled_stays_due() {
        let mut t = RecoveryThrottle::default();
        let t0 = Instant::now();
        assert!(!t.record_failure("regular", t0));
        assert!(!t.record_failure("regular", t0));
        assert!(t.record_failure("regular", t0));
        // Not claimed (already pending): the next failure is still due, no cool-down.
        assert!(t.record_failure("regular", t0 + Duration::from_secs(5)));
        t.mark_recovered("regular", t0 + Duration::from_secs(5));
        assert!(!t.record_failure("regular", t0 + Duration::from_secs(10)));
    }

    #[test]
    fn resyncs_are_budgeted() {
        let used = AtomicU32::new(0);
        let granted = (0..10).filter(|_| take_resync(&used)).count();
        assert_eq!(granted, MAX_RESYNCS as usize);
    }

    #[test]
    fn pending_collections_are_not_rescheduled_and_cost_no_budget() {
        let used = AtomicU32::new(0);
        let mut pending = Vec::new();
        let reg = || vec![WAPatchName::Regular];
        assert_eq!(plan_resync(&mut pending, &used, reg()).len(), 1);
        for _ in 0..10 {
            assert!(plan_resync(&mut pending, &used, reg()).is_empty());
        }
        assert_eq!(used.load(Ordering::SeqCst), 1);
        let both = vec![WAPatchName::Regular, WAPatchName::RegularLow];
        let next = plan_resync(&mut pending, &used, both);
        assert_eq!(next.iter().map(WAPatchName::as_str).collect::<Vec<_>>(), vec!["regular_low"]);
        assert_eq!(used.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn three_failures_within_two_minutes_trigger_one_recovery_per_half_hour() {
        let mut t = RecoveryThrottle::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!t.record_failure("regular", at(0)));
        assert!(!t.record_failure("regular", at(30)));
        assert!(t.record_failure("regular", at(60)));
        t.mark_recovered("regular", at(60));
        // Within the next half hour nothing triggers again, however often it fails.
        for s in (61..600).step_by(20) {
            assert!(!t.record_failure("regular", at(s)));
        }
        assert!(!t.record_failure("regular", at(25 * 60)));
        // After the interval a fresh burst triggers again.
        let later = 60 + 31 * 60;
        assert!(!t.record_failure("regular", at(later)));
        assert!(!t.record_failure("regular", at(later + 10)));
        assert!(t.record_failure("regular", at(later + 20)));
        t.mark_recovered("regular", at(later + 20));
        assert!(!t.record_failure("regular", at(later + 30)));
    }

    #[test]
    fn failures_spread_over_more_than_two_minutes_do_not_trigger() {
        let mut t = RecoveryThrottle::default();
        let t0 = Instant::now();
        for i in 0..10u64 {
            assert!(!t.record_failure("regular", t0 + Duration::from_secs(i * 70)));
        }
    }

    #[test]
    fn collections_are_throttled_independently() {
        let mut t = RecoveryThrottle::default();
        let t0 = Instant::now();
        assert!(!t.record_failure("regular", t0));
        assert!(!t.record_failure("regular_low", t0));
        assert!(!t.record_failure("regular", t0));
        assert!(!t.record_failure("regular_low", t0));
        assert!(t.record_failure("regular", t0));
        assert!(t.record_failure("regular_low", t0));
        t.mark_recovered("regular", t0);
        assert!(!t.record_failure("regular", t0));
        assert!(t.record_failure("regular_low", t0));
    }

    #[test]
    fn claim_pending_skips_collections_already_pending() {
        let mut pending = vec!["regular".to_string()];
        let both = vec![WAPatchName::Regular, WAPatchName::RegularLow];
        let fresh = claim_pending(&mut pending, both);
        assert_eq!(fresh.iter().map(WAPatchName::as_str).collect::<Vec<_>>(), vec!["regular_low"]);
        assert_eq!(pending, vec!["regular".to_string(), "regular_low".to_string()]);
    }

    #[test]
    fn exhausted_budget_marks_nothing_pending() {
        let used = AtomicU32::new(MAX_RESYNCS);
        let mut pending = Vec::new();
        assert!(plan_resync(&mut pending, &used, vec![WAPatchName::Regular]).is_empty());
        assert!(pending.is_empty());
    }
}
