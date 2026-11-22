//! Message translation: the "Translate" menu entry, the translation shown under
//! a bubble, the Settings section (target language, Manual / Automatic) and the
//! first-use dialog that asks for the target language.
//!
//! The engine does the work (cache + Google Translate); this module keeps the
//! per-message view state and renders it. Message text and translations are
//! never logged.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use slint::{ComponentHandle, ModelRc, VecModel};
use wa_engine::{Command, Translation};

use super::{App, with_app};
use crate::settings::EngineSettingsDto;
use crate::{LangItem, MainWindow, MsgItem, TranslateState};

/// A language the user can translate into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    /// Code Google Translate understands (ISO 639-1, plus `zh-CN`).
    pub code: &'static str,
    pub name: &'static str,
}

const fn lang(code: &'static str, name: &'static str) -> Language {
    Language { code, name }
}

const LANGUAGES: [Language; 47] = [
    lang("fa", "Persian"),
    lang("en", "English"),
    lang("ar", "Arabic"),
    lang("tr", "Turkish"),
    lang("de", "German"),
    lang("fr", "French"),
    lang("es", "Spanish"),
    lang("ru", "Russian"),
    lang("zh-CN", "Chinese"),
    lang("hi", "Hindi"),
    lang("ur", "Urdu"),
    lang("ku", "Kurdish"),
    lang("az", "Azerbaijani"),
    lang("ps", "Pashto"),
    lang("it", "Italian"),
    lang("pt", "Portuguese"),
    lang("ja", "Japanese"),
    lang("ko", "Korean"),
    lang("nl", "Dutch"),
    lang("sv", "Swedish"),
    lang("uk", "Ukrainian"),
    lang("he", "Hebrew"),
    lang("id", "Indonesian"),
    lang("ms", "Malay"),
    lang("pl", "Polish"),
    lang("cs", "Czech"),
    lang("el", "Greek"),
    lang("ro", "Romanian"),
    lang("hu", "Hungarian"),
    lang("bg", "Bulgarian"),
    lang("sr", "Serbian"),
    lang("hr", "Croatian"),
    lang("da", "Danish"),
    lang("fi", "Finnish"),
    lang("no", "Norwegian"),
    lang("sk", "Slovak"),
    lang("th", "Thai"),
    lang("vi", "Vietnamese"),
    lang("bn", "Bengali"),
    lang("ta", "Tamil"),
    lang("sw", "Swahili"),
    lang("uz", "Uzbek"),
    lang("kk", "Kazakh"),
    lang("tg", "Tajik"),
    lang("hy", "Armenian"),
    lang("ka", "Georgian"),
    lang("tk", "Turkmen"),
];

/// Every offered language, in display order (shared with the composer).
pub fn languages() -> &'static [Language] {
    &LANGUAGES
}

/// The language for a code. Case-insensitive; a region form such as `pt-BR`
/// falls back to its base language, and a few legacy codes are understood.
pub fn find_language(code: &str) -> Option<&'static Language> {
    let code = code.trim();
    let exact = |c: &str| LANGUAGES.iter().find(|l| l.code.eq_ignore_ascii_case(c));
    if let Some(l) = exact(code) {
        return Some(l);
    }
    let base = code.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    match base.as_str() {
        "zh" => exact("zh-CN"),
        "iw" => exact("he"),
        "nb" | "nn" => exact("no"),
        "ckb" | "kmr" => exact("ku"),
        "prs" | "pes" => exact("fa"),
        b => exact(b),
    }
}

/// English name of a language code, if it is one of the offered languages.
pub fn language_name(code: &str) -> Option<&'static str> {
    find_language(code).map(|l| l.name)
}

/// The languages whose name contains `query` (or whose code equals it).
pub fn filter_languages(query: &str) -> Vec<&'static Language> {
    let q = query.trim().to_lowercase();
    LANGUAGES
        .iter()
        .filter(|l| q.is_empty() || l.name.to_lowercase().contains(&q) || l.code.eq_ignore_ascii_case(&q))
        .collect()
}

/// Maps a Windows language tag (`fa-IR`, `zh-Hans-CN`) to an offered language code.
pub fn windows_language_to_code(tag: &str) -> Option<&'static str> {
    find_language(tag).map(|l| l.code)
}

/// The Windows display language, from the registry (first preferred UI
/// language, else the regional format).
fn windows_ui_language() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let first_of = |path: &str, name: &str| -> Option<String> {
        hkcu.open_subkey(path).ok()?.get_value::<Vec<String>, _>(name).ok()?.into_iter().next()
    };
    first_of(r"Control Panel\Desktop", "PreferredUILanguages")
        .or_else(|| first_of(r"Control Panel\Desktop\MuiCached", "MachinePreferredUILanguages"))
        .or_else(|| hkcu.open_subkey(r"Control Panel\International").ok()?.get_value::<String, _>("LocaleName").ok())
}

/// The language the first-use dialog starts on: the Windows display language
/// when it is offered.
fn preselected_language() -> Option<&'static str> {
    windows_ui_language().and_then(|t| windows_language_to_code(&t))
}

// ---- per-message view state (pure) --------------------------------------------------

/// What the bubble of one message shows about its translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrState {
    /// Asked for, no answer yet.
    Pending,
    /// Translated; `show` = the translation is visible under the original.
    /// `orig` is the hash of the text that was translated (`None` until the
    /// row's text is known), to notice that the message was edited since.
    Ready { text: String, source: String, show: bool, orig: Option<u64> },
    /// The message is already in the target language (engine reports
    /// source == target); nothing to show but a note.
    Same(String),
    Failed(String),
}

/// What choosing "Translate" in the menu does for a message in this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Ask the engine.
    Request,
    /// Switch between translation and original.
    Toggle,
    /// A request is already running.
    Nothing,
}

pub fn menu_action(prev: Option<&TrState>) -> MenuAction {
    match prev {
        None | Some(TrState::Failed(_)) | Some(TrState::Same(_)) => MenuAction::Request,
        Some(TrState::Ready { .. }) => MenuAction::Toggle,
        Some(TrState::Pending) => MenuAction::Nothing,
    }
}

/// A translation arrived (asked for, cached or pushed by auto-translate). It is
/// shown by default, but a translation the user already hid stays hidden.
pub fn on_arrival(prev: Option<&TrState>, t: &Translation) -> TrState {
    if t.source_lang == t.target_lang {
        return TrState::Same(t.source_lang.clone());
    }
    let show = !matches!(prev, Some(TrState::Ready { show: false, .. }));
    TrState::Ready { text: t.text.clone(), source: t.source_lang.clone(), show, orig: None }
}

/// Whether a translation still matches the profile's current target language.
/// A reply that was in flight when the target changed does not.
pub fn matches_target(t: &Translation, target: Option<&str>) -> bool {
    target.is_some_and(|c| c.eq_ignore_ascii_case(&t.target_lang))
}

pub fn text_hash(text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Checks a state against the row's current original text. A translation of a
/// text that has since been edited is dropped (the row shows the original and
/// offers "Translate" again); one whose source text was not known yet adopts
/// `text`. Other states are kept.
pub fn reconcile(state: Option<TrState>, text: &str) -> Option<TrState> {
    match state {
        Some(TrState::Ready { orig: Some(h), .. }) if h != text_hash(text) => None,
        Some(TrState::Ready { text: t, source, show, orig: None }) => Some(TrState::Ready { text: t, source, show, orig: Some(text_hash(text)) }),
        other => other,
    }
}

/// A request failed. Only a running request can fail; a translation that is
/// already there is kept.
pub fn on_failure(prev: Option<&TrState>, error: &str) -> Option<TrState> {
    match prev {
        Some(TrState::Pending) => Some(TrState::Failed(error.chars().take(140).collect())),
        _ => None,
    }
}

pub fn on_toggle(prev: &TrState) -> TrState {
    match prev {
        TrState::Ready { text, source, show, orig } => TrState::Ready { text: text.clone(), source: source.clone(), show: !show, orig: *orig },
        other => other.clone(),
    }
}

/// The row's translation fields: `state` as in `MsgItem.tr-state`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowView {
    pub state: i32,
    pub text: String,
    pub note: String,
}

pub fn view(s: Option<&TrState>) -> RowView {
    match s {
        None => RowView::default(),
        Some(TrState::Pending) => RowView { state: 1, ..RowView::default() },
        Some(TrState::Ready { text, source, show: true, .. }) => RowView {
            state: 2,
            text: text.clone(),
            note: language_name(source).map_or_else(|| "Translated".to_string(), |n| format!("Translated from {n}")),
        },
        Some(TrState::Ready { show: false, .. }) => RowView { state: 4, ..RowView::default() },
        Some(TrState::Failed(e)) => RowView { state: 3, text: String::new(), note: format!("Translation failed: {e}") },
        Some(TrState::Same(lang)) => RowView {
            state: 5,
            text: String::new(),
            note: language_name(lang).map_or_else(|| "Already in this language".to_string(), |n| format!("Already in {n}")),
        },
    }
}

// ---- controller state -----------------------------------------------------------------

/// (profile, chat, message id).
type Key = (String, String, String);

/// Most remembered states before those of other chats are dropped.
const MAX_STATES: usize = 2000;
/// Message ids per `FetchTranslations`.
const FETCH_CHUNK: usize = 200;

#[derive(Default)]
struct Store {
    states: HashMap<Key, TrState>,
    /// Chat whose rows were already asked about, and the ids asked.
    fetched_chat: Option<(String, String)>,
    fetched: HashSet<String>,
    /// The message the user chose "Translate" for while no language was set.
    waiting: Option<Key>,
}

impl Store {
    fn prune(&mut self, profile: &str, chat: &str) {
        if self.states.len() > MAX_STATES {
            self.states.retain(|k, _| k.0 == profile && k.1 == chat);
        }
    }

    fn forget_all(&mut self) {
        self.states.clear();
        self.fetched_chat = None;
        self.fetched.clear();
    }
}

thread_local! {
    static TR: RefCell<Store> = RefCell::new(Store::default());
}

fn language_model(query: &str) -> ModelRc<LangItem> {
    let items: Vec<LangItem> = filter_languages(query).into_iter().map(|l| LangItem { code: l.code.into(), name: l.name.into() }).collect();
    ModelRc::new(VecModel::from(items))
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<TranslateState>();
    g.set_languages(language_model(""));
    g.on_translate(|id| with_app(|a| a.translate_request(id.as_str())));
    g.on_retry(|id| with_app(|a| a.translate_request(id.as_str())));
    g.on_toggle(|id| with_app(|a| a.translate_toggle(id.as_str())));
    g.on_filter(|q| {
        with_app(|a| {
            if let Some(w) = a.win() {
                w.global::<TranslateState>().set_languages(language_model(q.as_str()));
            }
        });
    });
    g.on_set_target(|code| with_app(|a| a.translate_set_target(code.as_str())));
    g.on_set_auto(|on| with_app(|a| a.translate_set_auto(on)));
    g.on_dialog_save(|code| with_app(|a| a.translate_dialog_save(code.as_str())));
    g.on_dialog_cancel(|| with_app(App::translate_dialog_cancel));
}

impl App {
    /// Shows the profile's translation settings in the Settings page and dialog.
    pub(super) fn translate_sync(&self, e: &EngineSettingsDto) {
        let Some(win) = self.win() else { return };
        let g = win.global::<TranslateState>();
        let lang = e.translate_target.as_deref().and_then(find_language);
        g.set_target_code(lang.map_or("", |l| l.code).into());
        g.set_target_name(lang.map_or("", |l| l.name).into());
        g.set_auto(e.translate_auto);
    }

    /// The window switches to another profile: nothing of the old one stays.
    pub(super) fn translate_profile_changed(&self) {
        TR.with(|t| {
            let mut t = t.borrow_mut();
            t.forget_all();
            t.waiting = None;
        });
        if let Some(win) = self.win() {
            win.global::<TranslateState>().set_dialog_open(false);
        }
    }

    fn open_chat_key(&self, id: &str) -> Option<Key> {
        let p = self.p();
        let chat = p.st.borrow().selected.clone()?;
        Some((p.id.clone(), chat, id.to_string()))
    }

    /// "Translate" (or "Retry") on a message of the open chat.
    fn translate_request(&self, id: &str) {
        let Some(key) = self.open_chat_key(id) else { return };
        let prev = TR.with(|t| t.borrow().states.get(&key).cloned());
        match menu_action(prev.as_ref()) {
            MenuAction::Nothing => {}
            MenuAction::Toggle => self.translate_toggle(id),
            MenuAction::Request => {
                let target = self.p().settings.borrow().engine.translate_target.clone();
                if target.is_some() {
                    self.translate_begin(key);
                } else {
                    self.translate_ask_language(key);
                }
            }
        }
    }

    fn translate_begin(&self, key: Key) {
        let (_, chat_jid, id) = key.clone();
        TR.with(|t| t.borrow_mut().states.insert(key, TrState::Pending));
        self.send(Command::TranslateMessage { chat_jid, id: id.clone() });
        self.refresh_row(&id);
    }

    fn translate_toggle(&self, id: &str) {
        let Some(key) = self.open_chat_key(id) else { return };
        let changed = TR.with(|t| {
            let mut t = t.borrow_mut();
            let next = t.states.get(&key).map(on_toggle);
            match next {
                Some(n) => {
                    t.states.insert(key, n);
                    true
                }
                None => false,
            }
        });
        if changed {
            self.refresh_row(id);
        }
    }

    /// First use: no target language yet; the translation continues once one is saved.
    fn translate_ask_language(&self, key: Key) {
        let Some(win) = self.win() else { return };
        TR.with(|t| t.borrow_mut().waiting = Some(key));
        let g = win.global::<TranslateState>();
        g.set_languages(language_model(""));
        g.set_dialog_pick(preselected_language().unwrap_or("").into());
        g.set_dialog_open(true);
    }

    fn translate_close_dialog(&self) {
        if let Some(win) = self.win() {
            win.global::<TranslateState>().set_dialog_open(false);
            win.invoke_focus_composer();
        }
    }

    pub(super) fn translate_dialog_cancel(&self) {
        TR.with(|t| t.borrow_mut().waiting = None);
        self.translate_close_dialog();
    }

    fn translate_dialog_save(&self, code: &str) {
        let Some(lang) = find_language(code) else { return };
        let waiting = TR.with(|t| t.borrow_mut().waiting.take());
        self.translate_close_dialog();
        self.translate_apply_target(lang.code);
        if let Some(key) = waiting {
            if self.open_chat_key(&key.2).as_ref() == Some(&key) {
                self.translate_begin(key);
            }
        }
    }

    fn translate_set_target(&self, code: &str) {
        if let Some(lang) = find_language(code) {
            self.translate_apply_target(lang.code);
        }
    }

    /// Saves the target language, pushes it to the engine and drops the
    /// translations shown so far (they are in the previous language).
    fn translate_apply_target(&self, code: &str) {
        let changed = self.translate_update_settings(|e| e.translate_target = Some(code.to_string()));
        if !changed {
            return;
        }
        TR.with(|t| t.borrow_mut().forget_all());
        if self.p().st.borrow().selected.is_some() {
            self.rebuild_rows();
        }
    }

    fn translate_set_auto(&self, on: bool) {
        self.translate_update_settings(|e| e.translate_auto = on);
    }

    /// Applies `change` to the active profile's engine settings; saves and
    /// sends them when something changed.
    fn translate_update_settings(&self, change: impl FnOnce(&mut EngineSettingsDto)) -> bool {
        let p = self.p();
        let mut next = p.settings.borrow().clone();
        change(&mut next.engine);
        if next.engine == p.settings.borrow().engine {
            return false;
        }
        (p.send)(Command::UpdateSettings(next.engine.to_engine()));
        let engine = next.engine.clone();
        *p.settings.borrow_mut() = next;
        self.settings_dirty.borrow_mut().insert(p.id.clone());
        self.flush_settings();
        self.translate_sync(&engine);
        true
    }

    // ---- engine replies -------------------------------------------------------------

    /// `UiEvent::Translations`: answers, cached translations and auto-translations.
    pub(super) fn on_translations(&self, profile: &str, active: bool, chat_jid: &str, items: &[Translation]) {
        // Other profiles ask the engine again when they are entered.
        if !active {
            return;
        }
        let target = self.p().settings.borrow().engine.translate_target.clone();
        let items: Vec<&Translation> = items.iter().filter(|it| matches_target(it, target.as_deref())).collect();
        let open = self.p().st.borrow().selected.as_deref() == Some(chat_jid);
        TR.with(|t| {
            let mut t = t.borrow_mut();
            for it in &items {
                let key = (profile.to_string(), chat_jid.to_string(), it.id.clone());
                let next = on_arrival(t.states.get(&key), it);
                // The original text is only at hand for the open chat; other
                // chats adopt it when their rows are rendered.
                let next = if open {
                    let p = self.p();
                    let st = p.st.borrow();
                    let orig = st.msgs.iter().find(|m| m.id == it.id).map(|m| m.text.as_deref().unwrap_or(""));
                    match orig {
                        Some(text) => reconcile(Some(next), text),
                        None => Some(next),
                    }
                } else {
                    Some(next)
                };
                if let Some(next) = next {
                    t.states.insert(key, next);
                }
            }
            t.prune(profile, chat_jid);
        });
        if open {
            for it in &items {
                self.refresh_row(&it.id);
            }
        }
    }

    /// `UiEvent::TranslationFailed`.
    pub(super) fn on_translation_failed(&self, profile: &str, active: bool, chat_jid: &str, id: &str, error: &str) {
        if !active {
            return;
        }
        let key = (profile.to_string(), chat_jid.to_string(), id.to_string());
        let changed = TR.with(|t| {
            let mut t = t.borrow_mut();
            match on_failure(t.states.get(&key), error) {
                Some(n) => {
                    t.states.insert(key, n);
                    true
                }
                None => false,
            }
        });
        if changed && self.p().st.borrow().selected.as_deref() == Some(chat_jid) {
            self.refresh_row(id);
        }
    }

    // ---- rows -------------------------------------------------------------------------

    fn fill_row(item: &mut MsgItem, s: Option<&TrState>) {
        let v = view(s);
        item.tr_state = v.state;
        item.tr_text = v.text.into();
        item.tr_note = v.note.into();
    }

    /// Fills the translation fields of re-rendered rows and asks the engine for
    /// the cached translations of the rows it has not been asked about yet
    /// (chat opened, older messages loaded).
    pub(super) fn translate_decorate(&self, items: &mut [MsgItem]) {
        let p = self.p();
        let Some(chat) = p.st.borrow().selected.clone() else { return };
        let mut ask: Vec<String> = Vec::new();
        TR.with(|t| {
            let mut t = t.borrow_mut();
            let scope = (p.id.clone(), chat.clone());
            if t.fetched_chat.as_ref() != Some(&scope) {
                t.fetched_chat = Some(scope);
                t.fetched.clear();
            }
            for it in items.iter_mut().filter(|i| !i.is_sep && i.can_translate) {
                let key = (p.id.clone(), chat.clone(), it.id.to_string());
                let state = Self::reconciled(&mut t, &key, it.text.as_str());
                Self::fill_row(it, state.as_ref());
                if state.is_none() && t.fetched.insert(it.id.to_string()) {
                    ask.push(it.id.to_string());
                }
            }
        });
        for ids in ask.chunks(FETCH_CHUNK) {
            (p.send)(Command::FetchTranslations { chat_jid: chat.clone(), ids: ids.to_vec() });
        }
    }

    /// Same for one re-rendered row.
    pub(super) fn translate_decorate_one(&self, item: &mut MsgItem) {
        if !item.can_translate {
            return;
        }
        let Some(key) = self.open_chat_key(item.id.as_str()) else { return };
        let state = TR.with(|t| Self::reconciled(&mut t.borrow_mut(), &key, item.text.as_str()));
        Self::fill_row(item, state.as_ref());
    }

    /// The message's state after checking it against the row's current text;
    /// a translation of an edited message is forgotten.
    fn reconciled(t: &mut Store, key: &Key, text: &str) -> Option<TrState> {
        let state = reconcile(t.states.get(key).cloned(), text);
        match &state {
            Some(s) => t.states.insert(key.clone(), s.clone()),
            None => t.states.remove(key),
        };
        state
    }
}

#[cfg(debug_assertions)]
impl App {
    /// Debug aid for the demo (`WACLIENT_DEMO_UI=translate-shown|translate-states|
    /// translate-ask|translate-settings`): shows the translation UI without an engine.
    pub(super) fn demo_translate(&self, what: &str) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let alice = "111@s.whatsapp.net";
        let shown = Translation {
            id: "a17".into(),
            text: "Hello, take a look at this link https://example.ir/page, thank you".into(),
            source_lang: "fa".into(),
            target_lang: "en".into(),
        };
        match what {
            "translate-shown" => {
                self.select_chat(alice);
                self.on_translations(&p.id, true, alice, std::slice::from_ref(&shown));
            }
            "translate-states" => {
                self.select_chat(alice);
                self.on_translations(&p.id, true, alice, std::slice::from_ref(&shown));
                TR.with(|t| {
                    t.borrow_mut().states.insert((p.id.clone(), alice.to_string(), "a16".to_string()), TrState::Failed("No connection to the translation service".into()));
                });
                self.refresh_row("a16");
            }
            "translate-pending" => {
                self.select_chat(alice);
                TR.with(|t| {
                    t.borrow_mut().states.insert((p.id.clone(), alice.to_string(), "a17".to_string()), TrState::Pending);
                });
                self.refresh_row("a17");
            }
            "translate-ask" => {
                self.select_chat(alice);
                self.translate_request("a17");
            }
            "translate-settings" => {
                self.open_settings();
                win.set_settings_start_category(2);
                win.set_show_settings(true);
            }
            other => log::warn!("unknown translate demo: {other}"),
        }
        if what != "translate-settings" && what != "translate-ask" {
            slint::Timer::single_shot(std::time::Duration::from_millis(1200), || {
                with_app(|a| a.jump_to_message("111@s.whatsapp.net", "a17"));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(text: &str, source: &str) -> Translation {
        Translation { id: "m1".into(), text: text.into(), source_lang: source.into(), target_lang: "en".into() }
    }

    fn ready(show: bool) -> TrState {
        TrState::Ready { text: "hello".into(), source: "fr".into(), show, orig: None }
    }

    #[test]
    fn list_has_unique_codes_and_the_required_languages() {
        let all = languages();
        assert!(all.len() >= 40);
        for (i, l) in all.iter().enumerate() {
            assert!(!l.code.is_empty() && !l.name.is_empty());
            assert!(all[i + 1..].iter().all(|o| !o.code.eq_ignore_ascii_case(l.code)), "duplicate {}", l.code);
        }
        for code in ["fa", "en", "ar", "tr", "de", "fr", "es", "ru", "zh-CN", "hi", "ur", "ku", "az", "ps"] {
            assert!(find_language(code).is_some(), "missing {code}");
        }
        assert_eq!(all[0].code, "fa");
    }

    #[test]
    fn lookup_by_code() {
        assert_eq!(language_name("fa"), Some("Persian"));
        assert_eq!(language_name("FR"), Some("French"));
        assert_eq!(language_name("zh-cn"), Some("Chinese"));
        assert_eq!(language_name("zh"), Some("Chinese"));
        assert_eq!(language_name("pt-BR"), Some("Portuguese"));
        assert_eq!(language_name("iw"), Some("Hebrew"));
        assert_eq!(language_name("nb"), Some("Norwegian"));
        assert_eq!(language_name("xx"), None);
        assert_eq!(language_name(""), None);
    }

    #[test]
    fn filter_matches_names_and_codes() {
        assert_eq!(filter_languages("").len(), languages().len());
        let names: Vec<&str> = filter_languages(" PERS ").iter().map(|l| l.name).collect();
        assert_eq!(names, ["Persian"]);
        assert_eq!(filter_languages("zh-cn")[0].name, "Chinese");
        assert!(filter_languages("klingon").is_empty());
    }

    #[test]
    fn windows_languages_map_to_offered_codes() {
        assert_eq!(windows_language_to_code("fa-IR"), Some("fa"));
        assert_eq!(windows_language_to_code("en-US"), Some("en"));
        assert_eq!(windows_language_to_code("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(windows_language_to_code("zh-TW"), Some("zh-CN"));
        assert_eq!(windows_language_to_code("de_DE"), Some("de"));
        assert_eq!(windows_language_to_code("nb-NO"), Some("no"));
        assert_eq!(windows_language_to_code("ckb-IQ"), Some("ku"));
        assert_eq!(windows_language_to_code("qps-ploc"), None);
        assert_eq!(windows_language_to_code(""), None);
    }

    #[test]
    fn menu_action_follows_the_state() {
        assert_eq!(menu_action(None), MenuAction::Request);
        assert_eq!(menu_action(Some(&TrState::Failed("x".into()))), MenuAction::Request);
        assert_eq!(menu_action(Some(&TrState::Pending)), MenuAction::Nothing);
        assert_eq!(menu_action(Some(&ready(true))), MenuAction::Toggle);
        assert_eq!(menu_action(Some(&ready(false))), MenuAction::Toggle);
    }

    #[test]
    fn arrival_shows_the_translation_unless_the_user_hid_it() {
        assert_eq!(on_arrival(None, &tr("hello", "fr")), ready(true));
        assert_eq!(on_arrival(Some(&TrState::Pending), &tr("hello", "fr")), ready(true));
        assert_eq!(on_arrival(Some(&TrState::Failed("x".into())), &tr("hello", "fr")), ready(true));
        assert_eq!(on_arrival(Some(&ready(false)), &tr("hello", "fr")), ready(false));
        let same = Translation { id: "m".into(), text: "salam".into(), source_lang: "fa".into(), target_lang: "fa".into() };
        assert_eq!(on_arrival(Some(&TrState::Pending), &same), TrState::Same("fa".into()));
        assert_eq!(view(Some(&TrState::Same("fa".into()))).state, 5);
    }

    #[test]
    fn stale_target_replies_are_dropped() {
        assert!(matches_target(&tr("hello", "fr"), Some("en")));
        assert!(matches_target(&tr("hello", "fr"), Some("EN")));
        assert!(!matches_target(&tr("hello", "fr"), Some("de")));
        assert!(!matches_target(&tr("hello", "fr"), None));
        let same = Translation { id: "m".into(), text: "hi".into(), source_lang: "en".into(), target_lang: "en".into() };
        assert!(matches_target(&same, Some("en")));
    }

    #[test]
    fn an_edited_message_loses_its_translation() {
        let adopted = reconcile(Some(ready(true)), "bonjour");
        assert_eq!(adopted, Some(TrState::Ready { text: "hello".into(), source: "fr".into(), show: true, orig: Some(text_hash("bonjour")) }));
        assert_eq!(reconcile(adopted.clone(), "bonjour"), adopted);
        assert_eq!(reconcile(adopted, "bonsoir"), None);
        assert_eq!(reconcile(Some(TrState::Pending), "x"), Some(TrState::Pending));
        assert_eq!(reconcile(None, "x"), None);
    }

    #[test]
    fn only_a_running_request_can_fail() {
        assert_eq!(on_failure(Some(&TrState::Pending), "boom"), Some(TrState::Failed("boom".into())));
        assert_eq!(on_failure(Some(&ready(true)), "boom"), None);
        assert_eq!(on_failure(None, "boom"), None);
        let long = "x".repeat(500);
        match on_failure(Some(&TrState::Pending), &long) {
            Some(TrState::Failed(e)) => assert_eq!(e.chars().count(), 140),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn toggle_switches_between_translation_and_original() {
        assert_eq!(on_toggle(&ready(true)), ready(false));
        assert_eq!(on_toggle(&ready(false)), ready(true));
        assert_eq!(on_toggle(&TrState::Pending), TrState::Pending);
    }

    #[test]
    fn views_of_every_state() {
        assert_eq!(view(None), RowView::default());
        assert_eq!(view(Some(&TrState::Pending)).state, 1);
        let shown = view(Some(&ready(true)));
        assert_eq!((shown.state, shown.text.as_str(), shown.note.as_str()), (2, "hello", "Translated from French"));
        assert_eq!(view(Some(&ready(false))).state, 4);
        let unknown = TrState::Ready { text: "t".into(), source: "xx".into(), show: true, orig: None };
        assert_eq!(view(Some(&unknown)).note, "Translated");
        let failed = view(Some(&TrState::Failed("offline".into())));
        assert_eq!((failed.state, failed.note.as_str()), (3, "Translation failed: offline"));
    }

    #[test]
    fn pruning_keeps_only_the_open_chat_when_too_many() {
        let mut s = Store::default();
        for i in 0..=MAX_STATES {
            s.states.insert(("p".into(), format!("c{}", i % 3), format!("m{i}")), TrState::Pending);
        }
        s.prune("p", "c1");
        assert!(!s.states.is_empty());
        assert!(s.states.keys().all(|k| k.1 == "c1"));
    }
}
