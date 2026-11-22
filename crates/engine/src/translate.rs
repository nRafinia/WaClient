//! Message translation through Google's public translate endpoint, a local
//! cache, auto-translate of incoming messages, and language detection.
//!
//! Privacy: message text and translations are never logged; the log only
//! carries counts and HTTP status codes. All traffic goes through the
//! engine's own HTTP client, so the user's proxy applies.
//!
//! Requests are serialized process-wide (one at a time, spaced by
//! [`MIN_SPACING`]) so a burst of messages cannot hammer the service. A 429 or
//! 503 pauses the queue for [`BACKOFF`]; a network error or timeout pauses it
//! for the shorter [`OFFLINE_PAUSE`] (circuit breaker). Requests arriving or
//! waiting meanwhile fail fast. Manual requests give up waiting for their turn
//! after [`MANUAL_ACQUIRE_TIMEOUT`] and automatic ones step aside while a
//! manual request waits.
//!
//! "Already in the target language" (the service reports the same source
//! language as the target): a manual request emits a `Translation` carrying
//! the original text with `source_lang == target_lang` (the UI shows
//! "Already in <language>") and is not cached; auto mode stays silent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::oneshot;
use wa_store::{MessageKind, MessageRow, NewMessage, VIEW_ONCE_TEXT};
use whatsapp_rust::http::{HttpClient, HttpRequest};
use whatsapp_rust::prelude::Client;

use crate::api::{EngineSettings, Translation, UiEvent};
use crate::events::{Shared, now_secs};
use crate::media::lock;

const ENDPOINT: &str = "https://translate.googleapis.com/translate_a/single";
/// Longest text sent in one request, in characters.
const MAX_CHARS: usize = 5000;
/// Minimum time between the starts of two requests.
const MIN_SPACING: Duration = Duration::from_millis(300);
/// How long the queue stays paused after the service says it is overloaded.
const BACKOFF: Duration = Duration::from_secs(60);
/// How long the queue stays paused after a network error or timeout.
const OFFLINE_PAUSE: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a manual request waits for its turn in the queue.
const MANUAL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an automatic request steps aside when a manual one is waiting.
const AUTO_YIELD: Duration = Duration::from_millis(50);
/// Auto-translations waiting in the queue; more are dropped (a burst of
/// messages must not build an unbounded backlog).
const MAX_AUTO_PENDING: usize = 30;
/// Fewest letters a text needs before its language is detected.
const MIN_DETECT_LETTERS: usize = 12;
/// `whatlang` marks many correct Persian (and other Arabic-script) results
/// unreliable at chat-message lengths. From this many letters a confidence of
/// at least [`LONG_TEXT_CONFIDENCE`] is accepted as well; a wrong guess only
/// costs an extra translation request, since the service's own verdict decides
/// what is shown.
const LONG_TEXT_LETTERS: usize = 25;
const LONG_TEXT_CONFIDENCE: f64 = 0.5;

/// Why a translation request failed; `message` is what the UI shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranslateError {
    /// Network or proxy failure (or the queue is paused after one).
    Offline,
    /// The service said it is overloaded (HTTP 429/503).
    Busy,
    /// A manual request could not get its turn in time.
    QueueBusy,
    TimedOut,
    /// A 200 answer that could not be understood.
    BadReply,
    Http(u16),
    Cancelled,
}

impl TranslateError {
    fn message(self) -> String {
        match self {
            TranslateError::Offline => "Can't reach the translation service. Check your connection or proxy.".into(),
            TranslateError::Busy => "Translation service is busy, try again later".into(),
            TranslateError::QueueBusy => "Translation service is busy, try again in a moment".into(),
            TranslateError::TimedOut => "Translation timed out, try again".into(),
            TranslateError::BadReply => "Unexpected reply from the translation service".into(),
            TranslateError::Http(code) => format!("Translation service unavailable (HTTP {code})"),
            TranslateError::Cancelled => "Translation was cancelled".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Translated {
    text: String,
    source_lang: String,
}

// ---------------------------------------------------------------- pacing

/// Spacing bookkeeping of the request queue (pure: callers pass the current time).
#[derive(Debug, Default)]
struct Pacer {
    last_start: Option<Instant>,
}

impl Pacer {
    /// How long to wait before the next request may start.
    fn wait_before_next(&self, now: Instant) -> Duration {
        self.last_start
            .map(|t| (t + MIN_SPACING).saturating_duration_since(now))
            .unwrap_or_default()
    }

    fn record_start(&mut self, now: Instant) {
        self.last_start = Some(now);
    }

}

/// Pause of the queue after an overload answer or a network failure. Kept
/// apart from [`PACER`] so it can be read while a request holds the queue.
#[derive(Debug, Default)]
struct Breaker {
    paused_until: Option<Instant>,
    error: Option<TranslateError>,
}

impl Breaker {
    /// The error requests fail with right now, or `None` when not paused.
    fn blocked(&self, now: Instant) -> Option<TranslateError> {
        match (self.paused_until, self.error) {
            (Some(until), Some(error)) if now < until => Some(error),
            _ => None,
        }
    }

    /// Pauses after `error` (`Busy` for [`BACKOFF`], anything else for [`OFFLINE_PAUSE`]).
    fn trip(&mut self, error: TranslateError, now: Instant) {
        let pause = if error == TranslateError::Busy { BACKOFF } else { OFFLINE_PAUSE };
        self.paused_until = Some(now + pause);
        self.error = Some(error);
    }
}

/// Held for the whole request so only one runs at a time.
static PACER: tokio::sync::Mutex<Pacer> = tokio::sync::Mutex::const_new(Pacer { last_start: None });
static BREAKER: Mutex<Breaker> = Mutex::new(Breaker { paused_until: None, error: None });
/// Manual requests waiting for their turn; automatic ones step aside meanwhile.
static MANUAL_WAITING: AtomicUsize = AtomicUsize::new(0);

/// How long a request may wait for its turn; `None` = as long as it takes
/// (automatic requests are silent, so waiting costs nobody).
fn acquire_timeout(manual: bool) -> Option<Duration> {
    manual.then_some(MANUAL_ACQUIRE_TIMEOUT)
}

/// Counts a waiting manual request until dropped.
struct ManualWaiter;

impl ManualWaiter {
    fn new() -> Self {
        MANUAL_WAITING.fetch_add(1, Ordering::AcqRel);
        ManualWaiter
    }
}

impl Drop for ManualWaiter {
    fn drop(&mut self) {
        MANUAL_WAITING.fetch_sub(1, Ordering::AcqRel);
    }
}

type Waiter = oneshot::Sender<Result<Translated, TranslateError>>;
type InFlight = HashMap<(String, String), Vec<Waiter>>;

/// Identical requests (same text and target) currently in flight, with the
/// callers that joined them instead of sending their own.
static IN_FLIGHT: Mutex<Option<InFlight>> = Mutex::new(None);

/// The caller that sends a request others may have joined; hands the result
/// (or a cancellation, if dropped early) to them.
struct Leader {
    key: (String, String),
    result: Option<Result<Translated, TranslateError>>,
}

impl Drop for Leader {
    fn drop(&mut self) {
        let waiters = lock(&IN_FLIGHT)
            .as_mut()
            .and_then(|m| m.remove(&self.key))
            .unwrap_or_default();
        let result = self.result.take().unwrap_or(Err(TranslateError::Cancelled));
        for w in waiters {
            let _ = w.send(result.clone());
        }
    }
}

/// Translates `text` into `target`, joining an identical request in flight.
async fn request(client: &Arc<Client>, text: &str, target: &str, manual: bool) -> Result<Translated, TranslateError> {
    let key = (text.to_string(), target.to_string());
    let joined = {
        let mut guard = lock(&IN_FLIGHT);
        let map = guard.get_or_insert_with(HashMap::new);
        match map.get_mut(&key) {
            Some(waiters) => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                Some(rx)
            }
            None => {
                map.insert(key.clone(), Vec::new());
                None
            }
        }
    };
    if let Some(rx) = joined {
        return rx.await.unwrap_or(Err(TranslateError::Cancelled));
    }
    let mut leader = Leader { key, result: None };
    let result = queued_fetch(&*client.http_client, text, target, manual).await;
    leader.result = Some(result.clone());
    result
}

fn blocked_now() -> Option<TranslateError> {
    lock(&BREAKER).blocked(Instant::now())
}

/// Takes the queue's turn. Manual requests give up after
/// [`MANUAL_ACQUIRE_TIMEOUT`]; automatic ones let waiting manual requests go first.
async fn acquire(manual: bool) -> Result<tokio::sync::MutexGuard<'static, Pacer>, TranslateError> {
    let Some(limit) = acquire_timeout(manual) else {
        loop {
            let guard = PACER.lock().await;
            if MANUAL_WAITING.load(Ordering::Acquire) == 0 {
                return Ok(guard);
            }
            // The mutex is FIFO: releasing and re-queueing puts the manual request ahead.
            drop(guard);
            tokio::time::sleep(AUTO_YIELD).await;
        }
    };
    let _waiting = ManualWaiter::new();
    tokio::time::timeout(limit, PACER.lock()).await.map_err(|_| TranslateError::QueueBusy)
}

/// Waits for the queue's turn, then performs one request.
async fn queued_fetch(
    http: &dyn HttpClient,
    text: &str,
    target: &str,
    manual: bool,
) -> Result<Translated, TranslateError> {
    if let Some(e) = blocked_now() {
        return Err(e);
    }
    let mut pacer = acquire(manual).await?;
    // The request ahead may have tripped the breaker while this one waited.
    if let Some(e) = blocked_now() {
        return Err(e);
    }
    let wait = pacer.wait_before_next(Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    pacer.record_start(Instant::now());
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, http.execute(build_request(text, target))).await {
        Err(_) => {
            log::info!("translation request timed out, pausing the queue");
            lock(&BREAKER).trip(TranslateError::Offline, Instant::now());
            return Err(TranslateError::TimedOut);
        }
        Ok(Err(_)) => {
            log::info!("translation request failed: network error, pausing the queue");
            lock(&BREAKER).trip(TranslateError::Offline, Instant::now());
            return Err(TranslateError::Offline);
        }
        Ok(Ok(r)) => r,
    };
    match response.status_code {
        200 => parse_response(&response.body).map_err(|_| {
            log::info!("translation response not understood");
            TranslateError::BadReply
        }),
        429 | 503 => {
            log::info!("translation service answered {}, pausing the queue", response.status_code);
            lock(&BREAKER).trip(TranslateError::Busy, Instant::now());
            Err(TranslateError::Busy)
        }
        status => {
            log::info!("translation service answered {status}");
            Err(TranslateError::Http(status))
        }
    }
}

// ------------------------------------------------------------- wire format

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn build_request(text: &str, target: &str) -> HttpRequest {
    let body = format!("client=gtx&dt=t&sl=auto&tl={}&q={}", percent_encode(target), percent_encode(text));
    HttpRequest::post(ENDPOINT)
        .with_header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
        .with_body(body.into_bytes())
}

/// Reads `[[["<piece>","<original>",...], ...], null, "<source lang>", ...]`.
fn parse_response(body: &[u8]) -> Result<Translated> {
    let root: serde_json::Value = serde_json::from_slice(body)?;
    let pieces = root.get(0).and_then(|v| v.as_array()).ok_or_else(|| anyhow!("no pieces"))?;
    let mut text = String::new();
    let mut found = false;
    for piece in pieces {
        // The last entry may carry only a transliteration (null first element).
        if let Some(s) = piece.get(0).and_then(|v| v.as_str()) {
            text.push_str(s);
            found = true;
        }
    }
    if !found {
        return Err(anyhow!("no translated text"));
    }
    let source_lang = root
        .get(2)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("no source language"))?
        .to_string();
    Ok(Translated { text, source_lang })
}

/// Cuts `text` to [`MAX_CHARS`] characters; the flag says whether it was cut.
fn cap_text(text: &str) -> (String, bool) {
    match text.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => (text[..i].to_string(), true),
        None => (text.to_string(), false),
    }
}

// ------------------------------------------------------------ language ids

/// Primary language subtag in the ISO 639-1 form used here ("zh-CN" -> "zh").
fn normalize_lang(code: &str) -> String {
    let primary = code.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    match primary.as_str() {
        "iw" => "he".into(),
        "nb" | "nn" => "no".into(),
        "jw" => "jv".into(),
        _ => primary,
    }
}

fn same_lang(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_lang(a), normalize_lang(b));
    !a.is_empty() && a == b
}

fn iso_639_1(lang: whatlang::Lang) -> &'static str {
    use whatlang::Lang::*;
    match lang {
        Epo => "eo",
        Eng => "en",
        Rus => "ru",
        Cmn => "zh",
        Spa => "es",
        Por => "pt",
        Ita => "it",
        Ben => "bn",
        Fra => "fr",
        Deu => "de",
        Ukr => "uk",
        Kat => "ka",
        Ara => "ar",
        Hin => "hi",
        Jpn => "ja",
        Heb => "he",
        Yid => "yi",
        Pol => "pl",
        Amh => "am",
        Jav => "jv",
        Kor => "ko",
        Nob => "no",
        Dan => "da",
        Swe => "sv",
        Fin => "fi",
        Tur => "tr",
        Nld => "nl",
        Hun => "hu",
        Ces => "cs",
        Ell => "el",
        Bul => "bg",
        Bel => "be",
        Mar => "mr",
        Kan => "kn",
        Ron => "ro",
        Slv => "sl",
        Hrv => "hr",
        Srp => "sr",
        Mkd => "mk",
        Lit => "lt",
        Lav => "lv",
        Est => "et",
        Tam => "ta",
        Vie => "vi",
        Urd => "ur",
        Tha => "th",
        Guj => "gu",
        Uzb => "uz",
        Pan => "pa",
        Aze => "az",
        Ind => "id",
        Tel => "te",
        Pes => "fa",
        Mal => "ml",
        Ori => "or",
        Mya => "my",
        Nep => "ne",
        Sin => "si",
        Khm => "km",
        Tuk => "tk",
        Aka => "ak",
        Zul => "zu",
        Sna => "sn",
        Afr => "af",
        Lat => "la",
        Slk => "sk",
        Cat => "ca",
        Tgl => "tl",
        Hye => "hy",
        Cym => "cy",
    }
}

/// Punctuation outside ASCII that real chat text uses.
const EXTRA_PUNCT: &str = "\u{bf}\u{a1}\u{60c}\u{61f}\u{61b}\u{3002}\u{3001}\u{2026}\u{ab}\u{bb}\u{2014}\u{2013}\u{2019}\u{2018}\u{201c}\u{201d}";

/// Text reduced to what carries language: URLs, @mentions, emoji and digits
/// are dropped. Punctuation stays: it separates words, and trigram detection
/// is markedly less reliable without it.
fn language_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if word.starts_with('@')
            || lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("www.")
        {
            continue;
        }
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphabetic() || c.is_ascii_punctuation() || EXTRA_PUNCT.contains(*c))
            .collect();
        if !cleaned.chars().any(char::is_alphabetic) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&cleaned);
    }
    out
}

/// Detected language of `text` as an ISO 639-1 code, or `None` when the text
/// is too short or ambiguous. Uses `whatlang` (trigram statistics); only
/// results the library marks reliable (or confident enough on a longer text,
/// see [`LONG_TEXT_LETTERS`]) are returned.
pub fn detect_language(text: &str) -> Option<String> {
    let cleaned = language_text(text);
    let letters = cleaned.chars().filter(|c| c.is_alphabetic()).count();
    if letters < MIN_DETECT_LETTERS {
        return None;
    }
    let info = whatlang::detect(&cleaned)?;
    let accepted = info.is_reliable() || (letters >= LONG_TEXT_LETTERS && info.confidence() >= LONG_TEXT_CONFIDENCE);
    accepted.then(|| iso_639_1(info.lang()).to_string())
}

// -------------------------------------------------------- message selection

/// Text a message offers for translation: bodies and media captions only.
fn translatable_text(row: &MessageRow) -> Option<&str> {
    if row.revoked || is_system_id(&row.id) {
        return None;
    }
    let kind_ok = matches!(
        row.kind,
        MessageKind::Text | MessageKind::Image | MessageKind::Video | MessageKind::Document
    );
    let text = row.text.as_deref()?.trim();
    (kind_ok && !text.is_empty() && text != VIEW_ONCE_TEXT).then_some(text)
}

/// Rows the app writes itself (disappearing-timer notices, group changes, calls).
fn is_system_id(id: &str) -> bool {
    ["EPH-", "GRP-", "CALL-"].iter().any(|p| id.starts_with(p))
}

/// The configured target, when set.
fn target_of(settings: &EngineSettings) -> Option<&str> {
    settings.translate_target.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// Whether a freshly stored incoming message should be translated
/// automatically; returns the target language when it should.
fn auto_decision<'a>(settings: &'a EngineSettings, msg: &NewMessage) -> Option<&'a str> {
    if !settings.translate_auto || msg.from_me || is_system_id(&msg.id) || msg.kind != MessageKind::Text {
        return None;
    }
    let target = target_of(settings)?;
    let text = msg.text.as_deref()?;
    if text == VIEW_ONCE_TEXT {
        return None;
    }
    let lang = detect_language(text)?;
    (!same_lang(&lang, target)).then_some(target)
}

// ------------------------------------------------------------------ entry points

/// Translates one stored message into the configured target. Replies with
/// `Translations` (cache hit or fresh) or `TranslationFailed`.
pub async fn translate_message(sh: &Shared, client: &Arc<Client>, chat_jid: &str, id: &str) {
    let fail = |error: &str| {
        sh.emit(UiEvent::TranslationFailed {
            chat_jid: chat_jid.to_string(),
            id: id.to_string(),
            error: error.to_string(),
        });
    };
    let settings = sh.settings();
    let Some(target) = target_of(&settings) else {
        return fail("Choose a translation language first");
    };
    let row = match sh.store.message(chat_jid, id) {
        Ok(Some(row)) => row,
        Ok(None) => return fail("Nothing to translate"),
        Err(e) => {
            log::error!("translate: message lookup failed: {e}");
            return fail("Translation failed");
        }
    };
    let Some(original) = translatable_text(&row) else {
        return fail("Nothing to translate");
    };
    match sh.store.translation_of(chat_jid, id, target) {
        Ok(Some(hit)) => {
            sh.emit(UiEvent::Translations {
                chat_jid: chat_jid.to_string(),
                items: vec![Translation {
                    id: id.to_string(),
                    text: hit.text,
                    source_lang: hit.source_lang,
                    target_lang: target.to_string(),
                }],
            });
            return;
        }
        Ok(None) => {}
        Err(e) => log::error!("translate: cache read failed: {e}"),
    }
    let (capped, truncated) = cap_text(original);
    let translated = match request(client, &capped, target, true).await {
        Ok(t) => t,
        Err(e) => return fail(&e.message()),
    };
    let item = if same_lang(&translated.source_lang, target) {
        Translation {
            id: id.to_string(),
            text: original.to_string(),
            source_lang: translated.source_lang,
            target_lang: target.to_string(),
        }
    } else {
        let mut text = translated.text;
        if truncated {
            text.push('\u{2026}');
        }
        if !text_unchanged(sh, chat_jid, id, original) {
            return fail("The message changed, try again");
        }
        store_translation(sh, chat_jid, id, target, &text, &translated.source_lang);
        Translation { id: id.to_string(), text, source_lang: translated.source_lang, target_lang: target.to_string() }
    };
    sh.emit(UiEvent::Translations { chat_jid: chat_jid.to_string(), items: vec![item] });
}

/// Whether the stored message still has the text that was translated (an
/// edit during the request would otherwise leave a stale cache row).
fn text_unchanged(sh: &Shared, chat_jid: &str, id: &str, original: &str) -> bool {
    matches!(
        sh.store.message(chat_jid, id),
        Ok(Some(now)) if now.text.as_deref().map(str::trim) == Some(original.trim())
    )
}

fn store_translation(sh: &Shared, chat_jid: &str, id: &str, target: &str, text: &str, source_lang: &str) {
    if let Err(e) = sh.store.put_translation(chat_jid, id, target, text, source_lang, now_secs()) {
        log::error!("translate: cache write failed: {e}");
    }
}

/// Cached translations only (no network) for `ids` into the current target.
pub fn fetch_cached(sh: &Shared, chat_jid: &str, ids: &[String]) {
    let settings = sh.settings();
    let items = match target_of(&settings) {
        None => Vec::new(),
        Some(target) => match sh.store.translations_for(chat_jid, ids, target) {
            Ok(rows) => rows
                .into_iter()
                .map(|r| Translation {
                    id: r.msg_id,
                    text: r.text,
                    source_lang: r.source_lang,
                    target_lang: target.to_string(),
                })
                .collect(),
            Err(e) => {
                log::error!("translate: cache read failed: {e}");
                Vec::new()
            }
        },
    };
    sh.emit(UiEvent::Translations { chat_jid: chat_jid.to_string(), items });
}

/// Translates free text (not cached). Replies with `TextTranslated`.
pub async fn translate_text(sh: &Shared, client: &Arc<Client>, request_id: u64, text: &str, target: &str) {
    let target = target.trim();
    let text = text.trim();
    let result = if text.is_empty() || target.is_empty() {
        Err("Nothing to translate".to_string())
    } else {
        let (capped, truncated) = cap_text(text);
        request(client, &capped, target, true)
            .await
            .map_err(|e| e.message())
            .map(|t| {
                let mut out = t.text;
                if truncated {
                    out.push('\u{2026}');
                }
                (out, t.source_lang)
            })
    };
    sh.emit(UiEvent::TextTranslated { request_id, result });
}

static AUTO_PENDING: AtomicUsize = AtomicUsize::new(0);

/// Releases an auto-translation slot when the task ends or is aborted.
struct PendingSlot;

impl Drop for PendingSlot {
    fn drop(&mut self) {
        AUTO_PENDING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Hook for a freshly stored live incoming message: queues an automatic
/// translation when auto mode applies. Failures are silent (nobody asked).
pub fn auto_translate(sh: &Shared, client: &Arc<Client>, msg: &NewMessage, settings: &EngineSettings) {
    let Some(target) = auto_decision(settings, msg) else {
        return;
    };
    if AUTO_PENDING.fetch_add(1, Ordering::AcqRel) >= MAX_AUTO_PENDING {
        AUTO_PENDING.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let slot = PendingSlot;
    let (sh, client) = (sh.clone(), client.clone());
    let (chat, id, text, target) = (
        msg.chat_jid.clone(),
        msg.id.clone(),
        msg.text.clone().unwrap_or_default(),
        target.to_string(),
    );
    let tasks = sh.tasks.clone();
    tasks.spawn(async move {
        let _slot = slot;
        run_auto(&sh, &client, &chat, &id, &text, &target).await;
    });
}

async fn run_auto(sh: &Shared, client: &Arc<Client>, chat: &str, id: &str, text: &str, target: &str) {
    let (capped, truncated) = cap_text(text.trim());
    let Ok(translated) = request(client, &capped, target, false).await else {
        return;
    };
    if same_lang(&translated.source_lang, target) {
        return;
    }
    let mut out = translated.text;
    if truncated {
        out.push('\u{2026}');
    }
    if !text_unchanged(sh, chat, id, text) {
        return;
    }
    store_translation(sh, chat, id, target, &out, &translated.source_lang);
    sh.emit(UiEvent::Translations {
        chat_jid: chat.to_string(),
        items: vec![Translation {
            id: id.to_string(),
            text: out,
            source_lang: translated.source_lang,
            target_lang: target.to_string(),
        }],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Translated> {
        parse_response(json.as_bytes())
    }

    #[test]
    fn parses_multiple_pieces_and_line_breaks() {
        let t = parse(
            r#"[[["Hello.\n","Hola.\n",null,null,3],["How are you?","¿Cómo estás?",null,null,3],[null,null,"Hola"]],null,"es"]"#,
        )
        .unwrap();
        assert_eq!(t.text, "Hello.\nHow are you?");
        assert_eq!(t.source_lang, "es");
    }

    #[test]
    fn odd_shapes_are_errors_not_panics() {
        for body in [
            "",
            "null",
            "{}",
            "[]",
            "[null]",
            "[[]]",
            r#"[[[null]],null,"es"]"#,
            r#"[[["a","b"]],null]"#,
            r#"[[["a","b"]],null,5]"#,
            r#"[[["a","b"]],null,""]"#,
            "[1,2,3]",
            "not json",
        ] {
            assert!(parse(body).is_err(), "{body}");
        }
    }

    #[test]
    fn request_body_is_form_encoded() {
        let req = build_request("a b&c=é\n", "fa");
        assert_eq!(req.method, "POST");
        let body = String::from_utf8(req.body.unwrap().to_vec()).unwrap();
        assert_eq!(body, "client=gtx&dt=t&sl=auto&tl=fa&q=a%20b%26c%3D%C3%A9%0A");
    }

    #[test]
    fn long_text_is_capped_on_a_char_boundary() {
        let (short, cut) = cap_text("héllo");
        assert_eq!((short.as_str(), cut), ("héllo", false));
        let long = "é".repeat(MAX_CHARS + 10);
        let (capped, cut) = cap_text(&long);
        assert!(cut);
        assert_eq!(capped.chars().count(), MAX_CHARS);
        assert!(!cap_text(&"x".repeat(MAX_CHARS)).1);
    }

    #[test]
    fn pacer_spaces_requests() {
        let t0 = Instant::now();
        let mut p = Pacer::default();
        assert_eq!(p.wait_before_next(t0), Duration::ZERO);
        p.record_start(t0);
        assert_eq!(p.wait_before_next(t0 + Duration::from_millis(100)), Duration::from_millis(200));
        assert_eq!(p.wait_before_next(t0 + MIN_SPACING), Duration::ZERO);
        assert_eq!(p.wait_before_next(t0 + Duration::from_secs(5)), Duration::ZERO);
    }

    #[test]
    fn breaker_pauses_then_expires() {
        let t0 = Instant::now();
        let mut b = Breaker::default();
        assert_eq!(b.blocked(t0), None);
        b.trip(TranslateError::Busy, t0);
        assert_eq!(b.blocked(t0 + Duration::from_secs(59)), Some(TranslateError::Busy));
        assert_eq!(b.blocked(t0 + BACKOFF), None);
        b.trip(TranslateError::Offline, t0);
        assert_eq!(b.blocked(t0 + Duration::from_secs(19)), Some(TranslateError::Offline));
        assert_eq!(b.blocked(t0 + OFFLINE_PAUSE), None);
    }

    #[test]
    fn only_manual_requests_stop_waiting() {
        assert_eq!(acquire_timeout(true), Some(MANUAL_ACQUIRE_TIMEOUT));
        assert_eq!(acquire_timeout(false), None);
    }

    #[test]
    fn error_messages() {
        let offline = "Can't reach the translation service. Check your connection or proxy.";
        assert_eq!(TranslateError::Offline.message(), offline);
        assert_eq!(TranslateError::TimedOut.message(), "Translation timed out, try again");
        assert_eq!(TranslateError::BadReply.message(), "Unexpected reply from the translation service");
        assert_eq!(TranslateError::Http(500).message(), "Translation service unavailable (HTTP 500)");
        assert_eq!(TranslateError::QueueBusy.message(), "Translation service is busy, try again in a moment");
    }

    #[test]
    fn detects_common_languages() {
        let cases = [
            ("This is a perfectly ordinary English sentence about the weather.", "en"),
            ("Hola, ¿cómo estás? Quería preguntarte si mañana podemos vernos.", "es"),
            ("Guten Morgen, ich hoffe, dass es dir heute gut geht und du Zeit hast.", "de"),
            ("Привет, как дела? Давай встретимся завтра вечером после работы, я хочу рассказать тебе много нового о нашем проекте.", "ru"),
            ("سلام، حالت چطوره؟ امروز می‌خواستم باهات درباره کارهای فردا صحبت کنم", "fa"),
            ("مرحبا، كيف حالك اليوم؟ أردت أن أسألك عن موعد اجتماعنا غدا في المكتب", "ar"),
        ];
        for (text, lang) in cases {
            assert_eq!(detect_language(text).as_deref(), Some(lang), "{text}");
        }
    }

    #[test]
    fn short_or_non_linguistic_text_has_no_language() {
        for text in [
            "ok",
            "Hello",
            "",
            "😀😀😀😀😀😀😀😀😀😀😀😀😀😀",
            "https://example.com/some/long/path/to/a/page?x=1&y=2",
            "1234567890 98765 4321 555 000",
            "@123456789 @987654321 @555555555",
        ] {
            assert_eq!(detect_language(text), None, "{text}");
        }
    }

    #[test]
    fn urls_and_emoji_do_not_hide_the_language() {
        let text = "😀 https://example.com/x Hola, ¿cómo estás? Quería preguntarte si mañana podemos vernos 1234";
        assert_eq!(detect_language(text).as_deref(), Some("es"));
    }

    #[test]
    fn language_codes_compare_by_primary_subtag() {
        assert!(same_lang("zh-CN", "zh"));
        assert!(same_lang("iw", "he"));
        assert!(same_lang("EN", "en"));
        assert!(!same_lang("fa", "ar"));
        assert!(!same_lang("", ""));
    }

    fn incoming(id: &str, text: &str) -> NewMessage {
        NewMessage {
            chat_jid: "1@s.whatsapp.net".into(),
            id: id.into(),
            text: Some(text.into()),
            kind: MessageKind::Text,
            ..Default::default()
        }
    }

    fn settings(target: Option<&str>, auto: bool) -> EngineSettings {
        EngineSettings {
            translate_target: target.map(Into::into),
            translate_auto: auto,
            ..EngineSettings::default()
        }
    }

    const SPANISH: &str = "Hola, ¿cómo estás? Quería preguntarte si mañana podemos vernos.";
    const ENGLISH: &str = "This is a perfectly ordinary English sentence about the weather.";

    #[test]
    fn auto_decision_translates_foreign_text_only() {
        let msg = incoming("A1", SPANISH);
        assert_eq!(auto_decision(&settings(Some("en"), true), &msg), Some("en"));
        assert_eq!(auto_decision(&settings(Some("es"), true), &msg), None);
        assert_eq!(auto_decision(&settings(Some("en"), false), &msg), None);
        assert_eq!(auto_decision(&settings(None, true), &msg), None);
        assert_eq!(auto_decision(&settings(Some("  "), true), &msg), None);
        assert_eq!(auto_decision(&settings(Some("es"), true), &incoming("A2", ENGLISH)), Some("es"));
        assert_eq!(auto_decision(&settings(Some("en"), true), &incoming("A3", "ok")), None);
    }

    #[test]
    fn auto_decision_skips_own_system_media_and_view_once() {
        let s = settings(Some("en"), true);
        let mut own = incoming("A1", SPANISH);
        own.from_me = true;
        assert_eq!(auto_decision(&s, &own), None);
        for id in ["EPH-1", "GRP-1-0", "CALL-1"] {
            assert_eq!(auto_decision(&s, &incoming(id, SPANISH)), None, "{id}");
        }
        let mut image = incoming("A2", SPANISH);
        image.kind = MessageKind::Image;
        assert_eq!(auto_decision(&s, &image), None);
        assert_eq!(auto_decision(&s, &incoming("A3", VIEW_ONCE_TEXT)), None);
    }

    fn row(kind: MessageKind, text: Option<&str>) -> MessageRow {
        MessageRow { id: "m".into(), kind, text: text.map(Into::into), ..Default::default() }
    }

    #[test]
    fn only_text_and_captions_are_translatable() {
        assert_eq!(translatable_text(&row(MessageKind::Text, Some(" hi "))), Some("hi"));
        assert_eq!(translatable_text(&row(MessageKind::Image, Some("caption"))), Some("caption"));
        assert_eq!(translatable_text(&row(MessageKind::Image, None)), None);
        assert_eq!(translatable_text(&row(MessageKind::Text, Some("  "))), None);
        assert_eq!(translatable_text(&row(MessageKind::Sticker, Some("x"))), None);
        assert_eq!(translatable_text(&row(MessageKind::Other, Some(VIEW_ONCE_TEXT))), None);
        assert_eq!(translatable_text(&row(MessageKind::Text, Some(VIEW_ONCE_TEXT))), None);
        let mut revoked = row(MessageKind::Text, Some("x"));
        revoked.revoked = true;
        assert_eq!(translatable_text(&revoked), None);
        let mut system = row(MessageKind::Text, Some("x"));
        system.id = "GRP-1-0".into();
        assert_eq!(translatable_text(&system), None);
    }

    #[test]
    fn cache_round_trip_through_the_store() {
        let store = wa_store::Store::open_in_memory().unwrap();
        store.put_translation("c", "m1", "en", "hello", "es", 1).unwrap();
        let hit = store.translation_of("c", "m1", "en").unwrap().unwrap();
        assert_eq!((hit.text.as_str(), hit.source_lang.as_str()), ("hello", "es"));
        assert!(store.translation_of("c", "m1", "fa").unwrap().is_none());
    }
}
