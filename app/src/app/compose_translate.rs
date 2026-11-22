//! "Translate before sending": a button of the composer translates the typed
//! text into the chat's language and puts the result back into the composer.
//! Nothing is ever sent automatically; the strip above the composer offers Undo.
//!
//! The language of a chat is detected from its recent incoming texts (or taken
//! from the user's per-chat choice). Replies are matched by request id, so an
//! answer that arrives after the chat was left is ignored. The decisions are
//! pure and tested below; `App` only carries them out.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_engine::{Command, detect_language};
use wa_store::MessageKind;

use super::{App, ProfileState, translate_ui, with_app};
use crate::{ComposeLang, ComposeTranslate, MainWindow};

/// Incoming texts that are looked at to find the chat's language.
const SAMPLE_MESSAGES: usize = 20;
/// Rows read from the store to find that many incoming texts.
const FETCH_ROWS: u32 = 60;

fn language_name(code: &str) -> String {
    translate_ui::language_name(code).map_or_else(|| code.to_string(), str::to_string)
}

// ---- pure parts --------------------------------------------------------------------

/// The language most samples agree on. `samples` are ordered newest first; a
/// tie goes to the language whose newest sample is more recent. Samples that
/// could not be detected do not vote; no vote at all gives `None`.
fn majority(samples: &[Option<String>]) -> Option<String> {
    let mut tally: Vec<(&str, usize)> = Vec::new();
    for lang in samples.iter().flatten() {
        match tally.iter_mut().find(|(l, _)| *l == lang.as_str()) {
            Some((_, n)) => *n += 1,
            None => tally.push((lang.as_str(), 1)),
        }
    }
    // `max_by_key` keeps the last maximum, so walk newest-last to prefer the newest.
    tally.iter().rev().max_by_key(|(_, n)| *n).map(|(l, _)| (*l).to_string())
}

/// The chat's language: the detection over all samples joined together, else
/// the majority of the per-message detections.
fn chat_language(joined: Option<String>, each: &[Option<String>]) -> Option<String> {
    joined.or_else(|| majority(each))
}

/// What the strip above the composer shows.
#[derive(Debug, Clone, PartialEq, Default)]
enum Strip {
    #[default]
    None,
    /// Translated; Undo puts `original` back while the composer still holds `translated`.
    Done { original: String, translated: String, lang: String },
    /// The request failed; shown until the text is edited.
    Failed { text_at: String, message: String },
}

#[derive(Debug, Clone, PartialEq)]
struct Pending {
    id: u64,
    profile: String,
    chat: String,
    original: String,
    target: String,
}

/// How a reply was handled.
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    /// Not the request we are waiting for.
    Stale,
    /// Replace the composer text by this.
    Applied(String),
    /// The text already is in the target language; nothing to replace.
    Same,
    Failed,
}

#[derive(Debug, Default)]
struct Translate {
    next_id: u64,
    pending: Option<Pending>,
    strip: Strip,
    /// The user's chosen target language per (profile, chat).
    overrides: HashMap<(String, String), String>,
}

impl Translate {
    /// Starts a request; `None` while another one is running.
    fn begin(&mut self, profile: &str, chat: &str, original: &str, target: &str) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.strip = Strip::None;
        self.pending = Some(Pending {
            id,
            profile: profile.to_string(),
            chat: chat.to_string(),
            original: original.to_string(),
            target: target.to_string(),
        });
        Some(id)
    }

    /// Handles a reply for the chat that is open now (`profile`, `chat`).
    fn finish(
        &mut self,
        profile: &str,
        chat: Option<&str>,
        id: u64,
        result: Result<(String, String), String>,
    ) -> Reply {
        let Some(p) = self.pending.as_ref().filter(|p| p.id == id) else { return Reply::Stale };
        if p.profile != profile || chat != Some(p.chat.as_str()) {
            self.pending = None;
            return Reply::Stale;
        }
        let Some(p) = self.pending.take() else { return Reply::Stale };
        match result {
            Err(message) => {
                self.strip = Strip::Failed { text_at: p.original, message };
                Reply::Failed
            }
            Ok((translated, source)) => {
                if source == p.target || translated.trim() == p.original.trim() {
                    self.strip = Strip::Failed {
                        text_at: p.original,
                        message: format!("Already in {}", language_name(&p.target)),
                    };
                    return Reply::Same;
                }
                self.strip = Strip::Done { original: p.original, translated: translated.clone(), lang: p.target };
                Reply::Applied(translated)
            }
        }
    }

    /// The composer text changed: a strip about other text is out of date.
    fn edited(&mut self, text: &str) {
        let stale = match &self.strip {
            Strip::None => false,
            Strip::Done { translated, .. } => translated != text,
            Strip::Failed { text_at, .. } => text_at != text,
        };
        if stale {
            self.strip = Strip::None;
        }
    }

    /// Undo: the original text, if the composer still holds the translation.
    fn undo(&mut self, current: &str) -> Option<String> {
        match std::mem::take(&mut self.strip) {
            Strip::Done { original, translated, .. } if translated == current => Some(original),
            _ => None,
        }
    }

    /// The chat was left or the message sent: nothing carries over.
    fn reset(&mut self) {
        self.pending = None;
        self.strip = Strip::None;
    }

    /// (kind, text) for the strip: 0 none, 1 translated, 2 error.
    fn strip_view(&self) -> (i32, String) {
        match &self.strip {
            Strip::None => (0, String::new()),
            Strip::Done { lang, .. } => (1, format!("Translated to {}", language_name(lang))),
            Strip::Failed { message, .. } => (2, message.clone()),
        }
    }
}

thread_local! {
    static TR: RefCell<Translate> = RefCell::default();
}

// ---- controller --------------------------------------------------------------------

pub(super) fn wire(win: &MainWindow) {
    let t = win.global::<ComposeTranslate>();
    let langs: Vec<ComposeLang> = translate_ui::languages()
        .iter()
        .map(|l| ComposeLang { code: l.code.into(), name: l.name.into() })
        .collect();
    t.set_languages(ModelRc::new(VecModel::from(langs)));
    t.on_click(|| {
        let mut ask = false;
        with_app(|a| ask = a.tr_click());
        ask
    });
    t.on_pick(|code| with_app(|a| a.tr_pick(code.as_str())));
    t.on_undo(|| with_app(|a| a.tr_undo()));
    t.on_dismiss(|| with_app(|a| a.tr_dismiss()));
}

impl App {
    /// The button: translates into the chat's language. True when that language
    /// is unknown and the picker must open.
    fn tr_click(&self) -> bool {
        if self.chat_is_blocked_dm() || self.refuse_gated() {
            return false;
        }
        let (profile, chat) = {
            let p = self.p();
            let Some(chat) = p.st.borrow().selected.clone() else { return false };
            (p.id.clone(), chat)
        };
        let remembered = TR.with(|t| t.borrow().overrides.get(&(profile, chat.clone())).cloned());
        match remembered.or_else(|| self.tr_detect(&chat)) {
            Some(target) => {
                self.tr_start(&chat, &target);
                false
            }
            None => true,
        }
    }

    /// A language picked in the menu: remembered for this chat, then used.
    fn tr_pick(&self, code: &str) {
        let (profile, chat) = {
            let p = self.p();
            let Some(chat) = p.st.borrow().selected.clone() else { return };
            (p.id.clone(), chat)
        };
        if self.chat_is_blocked_dm() || self.refuse_gated() {
            return;
        }
        TR.with(|t| t.borrow_mut().overrides.insert((profile, chat.clone()), code.to_string()));
        self.tr_start(&chat, code);
    }

    /// The language of the other side, from the newest incoming texts.
    fn tr_detect(&self, chat: &str) -> Option<String> {
        let page = self.p().store.messages_page(chat, None, FETCH_ROWS).ok()?;
        let texts: Vec<&str> = page
            .iter()
            .rev()
            .filter(|m| !m.from_me && !m.revoked && m.kind == MessageKind::Text)
            .filter_map(|m| m.text.as_deref())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .take(SAMPLE_MESSAGES)
            .collect();
        if texts.is_empty() {
            return None;
        }
        let joined = detect_language(&texts.join("\n"));
        let each: Vec<Option<String>> = if joined.is_some() {
            Vec::new()
        } else {
            texts.iter().map(|t| detect_language(t)).collect()
        };
        // A detected language the translator does not offer must be picked by hand.
        let code = chat_language(joined, &each)?;
        translate_ui::find_language(&code).map(|l| l.code.to_string())
    }

    fn tr_start(&self, chat: &str, target: &str) {
        let Some(win) = self.win() else { return };
        let text = win.invoke_composer_text().to_string();
        if text.trim().is_empty() {
            return;
        }
        let profile = self.p().id.clone();
        let Some(id) = TR.with(|t| t.borrow_mut().begin(&profile, chat, &text, target)) else { return };
        self.tr_sync();
        self.send(Command::TranslateText { request_id: id, text, target: target.to_string() });
    }

    /// `UiEvent::TextTranslated`.
    pub(super) fn on_text_translated(
        &self,
        p: &ProfileState,
        active: bool,
        request_id: u64,
        result: Result<(String, String), String>,
    ) {
        let chat = active.then(|| p.st.borrow().selected.clone()).flatten();
        let reply = TR.with(|t| t.borrow_mut().finish(&p.id, chat.as_deref(), request_id, result));
        if matches!(reply, Reply::Stale) {
            self.tr_sync();
            return;
        }
        if matches!(reply, Reply::Failed) {
            log::warn!("composer translation failed");
        }
        if let (Reply::Applied(text), Some(win)) = (&reply, self.win()) {
            win.invoke_set_composer(text.as_str().into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
            self.draft_touch();
            win.invoke_focus_composer();
        }
        self.tr_sync();
    }

    fn tr_undo(&self) {
        let Some(win) = self.win() else { return };
        let current = win.invoke_composer_text().to_string();
        if let Some(original) = TR.with(|t| t.borrow_mut().undo(&current)) {
            win.invoke_set_composer(original.as_str().into(), i32::try_from(original.len()).unwrap_or(i32::MAX));
            self.draft_touch();
            win.invoke_focus_composer();
        }
        self.tr_sync();
    }

    fn tr_dismiss(&self) {
        TR.with(|t| t.borrow_mut().strip = Strip::None);
        self.tr_sync();
    }

    /// The composer text changed (typing, or any other source).
    pub(super) fn tr_edited(&self, text: &str) {
        TR.with(|t| t.borrow_mut().edited(text));
        self.tr_sync();
    }

    /// The chat was switched or the message sent.
    pub(super) fn tr_reset(&self) {
        TR.with(|t| t.borrow_mut().reset());
        self.tr_sync();
    }

    fn tr_sync(&self) {
        let Some(win) = self.win() else { return };
        let (busy, (kind, text)) = TR.with(|t| {
            let t = t.borrow();
            (t.pending.is_some(), t.strip_view())
        });
        let g = win.global::<ComposeTranslate>();
        g.set_busy(busy);
        g.set_strip_kind(kind);
        g.set_strip_text(text.into());
    }
}

#[cfg(debug_assertions)]
impl App {
    /// `WACLIENT_DEMO_UI=compose-translate`: a request runs through the real
    /// path and is answered like the engine would (`compose-translate-error`:
    /// with a failure; `compose-translate-busy`: not answered).
    pub(super) fn demo_compose_translate(&self, what: &str) {
        let Some(win) = self.win() else { return };
        self.select_chat("111@s.whatsapp.net");
        let text = "Hello, how are you? I will be there at six.";
        win.invoke_set_composer(text.into(), i32::try_from(text.len()).unwrap_or(i32::MAX));
        let profile = self.p().id.clone();
        let Some(id) = TR.with(|t| t.borrow_mut().begin(&profile, "111@s.whatsapp.net", text, "es")) else { return };
        self.tr_sync();
        let result = match what {
            "compose-translate-busy" => return,
            "compose-translate-error" => Err("Translation service unavailable".to_string()),
            _ => Ok(("Hola, ¿cómo estás? Estaré allí a las seis.".to_string(), "en".to_string())),
        };
        let p = self.p();
        self.on_text_translated(&p, true, id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(l: &str) -> Option<String> {
        Some(l.to_string())
    }

    #[test]
    fn majority_picks_the_most_common() {
        assert_eq!(majority(&[s("es"), s("en"), s("es")]), s("es"));
    }

    #[test]
    fn majority_tie_goes_to_the_newest() {
        assert_eq!(majority(&[s("en"), s("es")]), s("en"));
        assert_eq!(majority(&[s("es"), s("en"), s("en"), s("es")]), s("es"));
    }

    #[test]
    fn majority_ignores_undetected_and_may_be_empty() {
        assert_eq!(majority(&[None, s("fr"), None]), s("fr"));
        assert_eq!(majority(&[None, None]), None);
        assert_eq!(majority(&[]), None);
    }

    #[test]
    fn joined_detection_wins_over_the_majority() {
        assert_eq!(chat_language(s("de"), &[s("fr"), s("fr")]), s("de"));
        assert_eq!(chat_language(None, &[s("fr"), None, s("fr")]), s("fr"));
        assert_eq!(chat_language(None, &[None]), None);
    }

    fn ok(text: &str, source: &str) -> Result<(String, String), String> {
        Ok((text.to_string(), source.to_string()))
    }

    #[test]
    fn reply_is_matched_by_request_id() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        assert_eq!(t.finish("p", Some("c"), id + 1, ok("hola", "en")), Reply::Stale);
        assert!(t.pending.is_some(), "a stale reply must not cancel the request");
        assert_eq!(t.finish("p", Some("c"), id, ok("hola", "en")), Reply::Applied("hola".into()));
        assert!(t.pending.is_none());
        assert_eq!(t.finish("p", Some("c"), id, ok("hola", "en")), Reply::Stale);
    }

    #[test]
    fn reply_after_switching_chat_is_dropped() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        assert_eq!(t.finish("p", Some("other"), id, ok("hola", "en")), Reply::Stale);
        assert!(t.pending.is_none());
        assert_eq!(t.strip, Strip::None);
        let id = t.begin("p", "c", "hello", "es").unwrap();
        assert_eq!(t.finish("q", Some("c"), id, ok("hola", "en")), Reply::Stale);
        let id = t.begin("p", "c", "hello", "es").unwrap();
        assert_eq!(t.finish("p", None, id, ok("hola", "en")), Reply::Stale);
    }

    #[test]
    fn reset_drops_the_request_so_its_reply_is_stale() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        t.reset();
        assert_eq!(t.finish("p", Some("c"), id, ok("hola", "en")), Reply::Stale);
    }

    #[test]
    fn only_one_request_at_a_time() {
        let mut t = Translate::default();
        let first = t.begin("p", "c", "a", "es").unwrap();
        assert!(t.begin("p", "c", "b", "es").is_none());
        t.finish("p", Some("c"), first, ok("x", "en"));
        let second = t.begin("p", "c", "b", "es").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn errors_show_in_the_strip_until_the_text_changes() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        assert_eq!(t.finish("p", Some("c"), id, Err("offline".into())), Reply::Failed);
        assert_eq!(t.strip_view(), (2, "offline".to_string()));
        t.edited("hello");
        assert_eq!(t.strip_view().0, 2);
        t.edited("hello!");
        assert_eq!(t.strip_view().0, 0);
    }

    #[test]
    fn same_language_is_reported_not_applied() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "en").unwrap();
        assert_eq!(t.finish("p", Some("c"), id, ok("hello", "en")), Reply::Same);
        assert_eq!(t.strip_view(), (2, "Already in English".to_string()));
    }

    #[test]
    fn undo_restores_the_original_once() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        t.finish("p", Some("c"), id, ok("hola", "en"));
        assert_eq!(t.strip_view(), (1, "Translated to Spanish".to_string()));
        assert_eq!(t.undo("hola"), Some("hello".to_string()));
        assert_eq!(t.strip_view().0, 0);
        assert_eq!(t.undo("hola"), None);
    }

    #[test]
    fn undo_is_gone_after_an_edit_or_a_send() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        t.finish("p", Some("c"), id, ok("hola", "en"));
        t.edited("hola");
        assert_eq!(t.strip_view().0, 1, "an unchanged text keeps the strip");
        t.edited("hola!");
        assert_eq!(t.undo("hola!"), None);

        let id = t.begin("p", "c", "hello", "es").unwrap();
        t.finish("p", Some("c"), id, ok("hola", "en"));
        t.reset();
        assert_eq!(t.strip_view().0, 0);
    }

    #[test]
    fn undo_refuses_when_the_composer_no_longer_holds_the_translation() {
        let mut t = Translate::default();
        let id = t.begin("p", "c", "hello", "es").unwrap();
        t.finish("p", Some("c"), id, ok("hola", "en"));
        assert_eq!(t.undo("something else"), None);
    }

    #[test]
    fn language_names_fall_back_to_the_code() {
        assert_eq!(language_name("es"), "Spanish");
        assert_eq!(language_name("xx"), "xx");
    }
}
