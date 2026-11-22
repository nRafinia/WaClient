//! Profile-picture fetching with account-safe request pacing.
//!
//! Every lookup is an IQ to WhatsApp, so they are rate limited three ways:
//! a persisted "checked at" time (survives restarts), a global pace between
//! requests, and a back-off after repeated failures.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use whatsapp_rust::http::HttpRequest;
use whatsapp_rust::prelude::{Client, Jid};

use crate::api::UiEvent;
use crate::events::{Shared, now_secs};
use crate::jid::redact;
use crate::media::{avatar_path, lock, write_atomic_async};

/// A jid is not refetched sooner than this after a completed lookup, across
/// restarts (stored in `contacts.avatar_checked_at`).
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// In-process guard against re-queueing the same jid while it is being handled.
const REFETCH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// After a failed lookup (offline, timeout) a retry is allowed sooner.
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);
/// After this many consecutive failures for one jid the back-off grows.
const MAX_QUICK_RETRIES: u32 = 3;
const LONG_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Minimum spacing between two avatar requests (plus up to `PACE_JITTER_MS`).
const PACE: Duration = Duration::from_millis(400);
const PACE_JITTER_MS: u64 = 200;
/// Consecutive failures (any jid) that trip the circuit breaker.
const BREAKER_THRESHOLD: u32 = 5;
/// How long all avatar requests stay paused once the breaker tripped.
const BREAKER_PAUSE: Duration = Duration::from_secs(10 * 60);

/// Global circuit breaker: repeated failures suggest WhatsApp is throttling
/// us, so every avatar IQ stops for a while instead of making it worse.
#[derive(Debug, Default)]
pub(crate) struct Breaker {
    consecutive_errors: u32,
    open_until: Option<Instant>,
}

impl Breaker {
    /// Whether requests are currently paused. Closes (and clears the error
    /// count) once the pause has elapsed.
    pub(crate) fn is_open(&mut self, now: Instant) -> bool {
        match self.open_until {
            Some(until) if until > now => true,
            Some(_) => {
                self.reset();
                false
            }
            None => false,
        }
    }

    pub(crate) fn on_error(&mut self, now: Instant) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        if self.consecutive_errors >= BREAKER_THRESHOLD && self.open_until.is_none() {
            self.open_until = Some(now + BREAKER_PAUSE);
        }
    }

    pub(crate) fn on_success(&mut self) {
        self.consecutive_errors = 0;
    }

    pub(crate) fn reset(&mut self) {
        *self = Breaker::default();
    }
}

pub(crate) fn breaker_open(sh: &Shared) -> bool {
    lock(&sh.media.avatar_breaker).is_open(Instant::now())
}

/// Waits for an avatar permit and the next pacing slot, so on-demand picture
/// lookups share the budget of the background avatar fetches. `None` when the
/// breaker is open (WhatsApp seems to be throttling us) or the engine is
/// shutting down. Report the outcome of the request with [`record_outcome`].
pub(crate) async fn paced_slot(sh: &Shared) -> Option<tokio::sync::SemaphorePermit<'_>> {
    if breaker_open(sh) {
        return None;
    }
    let permit = sh.media.avatar_permits.acquire().await.ok()?;
    let wait = {
        let mut earliest = sh.media.avatar_pace.lock().await;
        let now = Instant::now();
        let (start, next) = next_slot(now, *earliest, jitter_ms());
        *earliest = next;
        start.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    (!breaker_open(sh)).then_some(permit)
}

/// Feeds the result of a request made under [`paced_slot`] to the breaker.
pub(crate) fn record_outcome(sh: &Shared, ok: bool) {
    let mut breaker = lock(&sh.media.avatar_breaker);
    if ok {
        breaker.on_success();
    } else {
        breaker.on_error(Instant::now());
    }
}

/// Drops a request while the breaker is open and frees its claim so the UI can
/// ask again later.
fn drop_paused(sh: &Shared, jid: &str) {
    log::debug!("avatar requests paused, dropping {}", redact(jid));
    lock(&sh.media.avatar_next).remove(jid);
}

/// Whether a lookup at `checked_at` is recent enough to skip a new one.
fn recently_checked(checked_at: i64, now: i64) -> bool {
    checked_at > 0 && now.saturating_sub(checked_at) < CHECK_INTERVAL_SECS
}

/// Delay before retrying a jid that failed `consecutive_errors` times in a row.
fn retry_delay(consecutive_errors: u32) -> Duration {
    if consecutive_errors >= MAX_QUICK_RETRIES {
        LONG_BACKOFF
    } else {
        RETRY_AFTER_ERROR
    }
}

/// Reserves the next request slot: returns when this request may start and
/// the new earliest start for the one after it.
fn next_slot(now: Instant, earliest: Instant, jitter_ms: u64) -> (Instant, Instant) {
    let start = now.max(earliest);
    (start, start + PACE + Duration::from_millis(jitter_ms % (PACE_JITTER_MS + 1)))
}

fn jitter_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) / 1000)
}

fn existing_avatar(sh: &Shared, jid: &str) -> Option<PathBuf> {
    sh.store
        .contact(jid)
        .ok()
        .flatten()
        .and_then(|c| c.avatar_path)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

/// Marks `jid` as being fetched; returns `false` if it was fetched recently
/// or is backing off.
fn claim(sh: &Shared, jid: &str) -> bool {
    let mut map = lock(&sh.media.avatar_next);
    let now = Instant::now();
    if map.get(jid).is_some_and(|next| *next > now) {
        return false;
    }
    map.insert(jid.to_string(), now + REFETCH_AFTER);
    true
}

fn record_error(sh: &Shared, jid: &str) {
    let count = {
        let mut errors = lock(&sh.media.avatar_errors);
        let count = errors.entry(jid.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    };
    lock(&sh.media.avatar_next).insert(jid.to_string(), Instant::now() + retry_delay(count));
}

fn record_success(sh: &Shared, jid: &str) {
    lock(&sh.media.avatar_errors).remove(jid);
    if let Err(e) = sh.store.mark_avatar_checked(jid, now_secs()) {
        log::error!("mark_avatar_checked failed: {e}");
    }
}

/// Fetches (or refreshes) the profile picture of `jid` and emits
/// [`UiEvent::AvatarReady`] when a picture is available.
pub async fn fetch(sh: Shared, client: Arc<Client>, jid: String) {
    fetch_inner_guarded(sh, client, jid, false).await;
}

async fn fetch_inner_guarded(sh: Shared, client: Arc<Client>, jid: String, force: bool) {
    let checked_at = sh
        .store
        .contact(&jid)
        .ok()
        .flatten()
        .map_or(0, |c| c.avatar_checked_at);
    let skip = (!force && recently_checked(checked_at, now_secs())) || !claim(&sh, &jid);
    if skip {
        if let Some(path) = existing_avatar(&sh, &jid) {
            sh.emit(UiEvent::AvatarReady { jid, path });
        }
        return;
    }
    if breaker_open(&sh) {
        drop_paused(&sh, &jid);
        return;
    }
    let Ok(_permit) = sh.media.avatar_permits.acquire().await else {
        log::error!("avatar semaphore closed");
        return;
    };
    let wait = {
        let mut earliest = sh.media.avatar_pace.lock().await;
        let now = Instant::now();
        let (start, next) = next_slot(now, *earliest, jitter_ms());
        *earliest = next;
        start.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    // The breaker may have tripped while this request was queued.
    if breaker_open(&sh) {
        drop_paused(&sh, &jid);
        return;
    }
    match fetch_one(&sh, &client, &jid).await {
        Ok(()) => {
            lock(&sh.media.avatar_breaker).on_success();
            record_success(&sh, &jid);
        }
        Err(e) => {
            log::debug!("avatar fetch for {} failed: {e:#}", redact(&jid));
            lock(&sh.media.avatar_breaker).on_error(Instant::now());
            record_error(&sh, &jid);
        }
    }
}

async fn fetch_one(sh: &Shared, client: &Client, jid: &str) -> anyhow::Result<()> {
    let parsed: Jid = jid
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid jid {jid:?}: {e}"))?;
    let Some(pic) = client.contacts().get_profile_picture(&parsed, true).await? else {
        return Ok(());
    };

    let known = sh.store.contact(jid)?;
    if known.as_ref().and_then(|c| c.avatar_id.as_deref()) == Some(pic.id.as_str())
        && let Some(path) = existing_avatar(sh, jid)
    {
        sh.emit(UiEvent::AvatarReady {
            jid: jid.to_string(),
            path,
        });
        return Ok(());
    }

    let response = client
        .http_client
        .execute(HttpRequest::get(pic.url.clone()))
        .await?;
    if response.status_code != 200 {
        anyhow::bail!("picture download answered HTTP {}", response.status_code);
    }
    let path = avatar_path(&sh.media.dir, jid);
    write_atomic_async(path.clone(), response.body).await?;
    sh.store
        .set_contact_avatar(jid, &pic.id, &path.to_string_lossy())?;
    sh.emit(UiEvent::AvatarReady {
        jid: jid.to_string(),
        path,
    });
    Ok(())
}

/// A contact changed its picture: if we ever fetched it, fetch it again (this
/// bypasses the 24 h check but still goes through pacing and back-off).
pub fn invalidate_and_refetch(sh: &Shared, client: &Arc<Client>, jid: String) {
    let in_memory = lock(&sh.media.avatar_next).remove(&jid).is_some();
    let persisted = sh
        .store
        .contact(&jid)
        .ok()
        .flatten()
        .is_some_and(|c| c.avatar_checked_at > 0);
    if in_memory || persisted {
        sh.tasks.spawn(fetch_inner_guarded(sh.clone(), client.clone(), jid, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_interval_is_24_hours() {
        let now = 1_000_000;
        assert!(!recently_checked(0, now));
        assert!(recently_checked(now - 60, now));
        assert!(recently_checked(now - CHECK_INTERVAL_SECS + 1, now));
        assert!(!recently_checked(now - CHECK_INTERVAL_SECS, now));
    }

    #[test]
    fn backs_off_for_an_hour_after_three_errors() {
        assert_eq!(retry_delay(1), RETRY_AFTER_ERROR);
        assert_eq!(retry_delay(2), RETRY_AFTER_ERROR);
        assert_eq!(retry_delay(3), LONG_BACKOFF);
        assert_eq!(retry_delay(10), LONG_BACKOFF);
    }

    #[test]
    fn breaker_opens_after_five_errors_and_closes_after_the_pause() {
        let t0 = Instant::now();
        let mut b = Breaker::default();
        for _ in 0..BREAKER_THRESHOLD - 1 {
            b.on_error(t0);
            assert!(!b.is_open(t0));
        }
        b.on_success();
        for _ in 0..BREAKER_THRESHOLD - 1 {
            b.on_error(t0);
        }
        assert!(!b.is_open(t0), "a success resets the streak");
        b.on_error(t0);
        assert!(b.is_open(t0));
        assert!(b.is_open(t0 + BREAKER_PAUSE - Duration::from_secs(1)));
        assert!(!b.is_open(t0 + BREAKER_PAUSE));
        b.on_error(t0 + BREAKER_PAUSE);
        assert!(!b.is_open(t0 + BREAKER_PAUSE), "count restarts after the pause");

        for _ in 0..BREAKER_THRESHOLD {
            b.on_error(t0);
        }
        assert!(b.is_open(t0));
        b.reset();
        assert!(!b.is_open(t0));
    }

    #[test]
    fn requests_are_spaced_by_pace_plus_bounded_jitter() {
        let t0 = Instant::now();
        let (s1, e1) = next_slot(t0, t0, 150);
        assert_eq!(s1, t0);
        assert_eq!(e1 - s1, PACE + Duration::from_millis(150));

        // A second request arriving immediately waits for the first slot.
        let (s2, e2) = next_slot(t0, e1, 0);
        assert_eq!(s2, e1);
        assert_eq!(e2 - s2, PACE);

        // Jitter never exceeds the bound; an idle gap does not accumulate.
        let later = e2 + Duration::from_secs(10);
        let (s3, e3) = next_slot(later, e2, 10_000);
        assert_eq!(s3, later);
        assert!(e3 - s3 >= PACE && e3 - s3 <= PACE + Duration::from_millis(PACE_JITTER_MS));
    }
}
