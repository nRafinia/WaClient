//! "Export chat…" (chat header menu): the dialog, the folder picker and the
//! worker thread that runs the export (see `crate::export`). Nothing leaves
//! the PC; the dialog only reports counts, never message text.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{Local, NaiveDate};
use slint::ComponentHandle;
use wa_store::ChatKind;

use super::{App, run_launcher, with_app};
use crate::export::{self, ChatCtx, Format, Job, Outcome, Range, folder_name, group_digits, parse_date, range_bounds};
use crate::{ExportState, MainWindow};

/// The chat and profile the open dialog exports from.
struct Target {
    chat_jid: String,
    chat_name: String,
    is_group: bool,
    profile: String,
}

/// A running export.
struct Active {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

thread_local! {
    static TARGET: RefCell<Option<Target>> = const { RefCell::new(None) };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

pub(super) fn wire(win: &MainWindow) {
    let g = win.global::<ExportState>();
    g.on_open_dialog(|| with_app(App::export_open));
    g.on_browse(|| with_app(App::export_browse));
    g.on_start(|| with_app(App::export_start));
    g.on_cancel(|| with_app(App::export_cancel));
    g.on_close(|| with_app(App::export_close));
    g.on_open_folder(|| with_app(App::export_open_folder));
}

/// `<Documents>\WaClient Export\<chat name> <date>`.
fn default_dest(documents: Option<PathBuf>, chat_name: &str, today: NaiveDate) -> Option<PathBuf> {
    Some(documents?.join("WaClient Export").join(folder_name(chat_name, today)))
}

fn documents_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        super::save_as::known_folder::path(&super::save_as::known_folder::DOCUMENTS)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Documents")))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Documents"))
    }
}

/// The range the dialog asks for, with its words for the file header.
fn read_range(kind: i32, from: &str, to: &str) -> Result<(Range, String), String> {
    match kind {
        1 => Ok((Range::Last30Days, "Last 30 days".to_string())),
        2 => {
            let (Some(from), Some(to)) = (parse_date(from), parse_date(to)) else {
                return Err("Enter both dates as YYYY-MM-DD".to_string());
            };
            Ok((Range::Custom { from, to }, format!("{} to {}", from.format("%Y-%m-%d"), to.format("%Y-%m-%d"))))
        }
        _ => Ok((Range::All, "All messages".to_string())),
    }
}

fn result_text(messages: u64, media_copied: usize, media_missing: usize, dest: &Path) -> String {
    let noun = if messages == 1 { "message" } else { "messages" };
    let mut text = format!("Exported {} {noun}", group_digits(messages));
    if media_copied > 0 {
        text.push_str(&format!(", {} media files copied", group_digits(media_copied as u64)));
    }
    if media_missing > 0 {
        text.push_str(&format!(", {} not downloaded", group_digits(media_missing as u64)));
    }
    text.push_str(&format!(".\n{}", dest.display()));
    text
}

impl App {
    pub(super) fn export_open(&self) {
        let Some(win) = self.win() else { return };
        let p = self.p();
        let Some(jid) = p.st.borrow().selected.clone() else { return };
        let (name, is_group) = match p.store.chat(&jid) {
            Ok(Some(c)) => (c.display_name, c.kind == ChatKind::Group),
            Ok(None) => (jid.split('@').next().unwrap_or("Chat").to_string(), jid.ends_with("@g.us")),
            Err(e) => {
                log::warn!("export: could not read the chat: {e}");
                self.show_banner(1, "Could not read the chat");
                return;
            }
        };
        let today = Local::now().date_naive();
        let dest = default_dest(documents_dir(), &name, today).map(|d| d.display().to_string()).unwrap_or_default();
        let g = win.global::<ExportState>();
        g.set_chat_name(name.clone().into());
        g.set_format(0);
        g.set_range(0);
        g.set_from_text(today.format("%Y-%m-%d").to_string().into());
        g.set_to_text(today.format("%Y-%m-%d").to_string().into());
        g.set_include_media(true);
        g.set_dest(dest.into());
        g.set_error("".into());
        g.set_running(false);
        g.set_finished(false);
        g.set_progress(0.0);
        TARGET.with(|t| *t.borrow_mut() = Some(Target { chat_jid: jid, chat_name: name, is_group, profile: p.id.clone() }));
        g.set_open(true);
    }

    fn export_browse(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ExportState>();
        let start = PathBuf::from(g.get_dest().as_str()).ancestors().find(|a| a.is_dir()).map(Path::to_path_buf);
        let name = TARGET.with(|t| t.borrow().as_ref().map(|t| t.chat_name.clone())).unwrap_or_default();
        let spawned = std::thread::Builder::new().name("export-folder".into()).spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Choose where to save the export");
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            let Some(picked) = dialog.pick_folder() else { return };
            // A folder of its own inside the chosen one, so the files never
            // land loose among the user's other documents.
            let dest = picked.join(folder_name(&name, Local::now().date_naive()));
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| {
                    if let Some(win) = a.win() {
                        win.global::<ExportState>().set_dest(dest.display().to_string().into());
                    }
                });
            });
            if let Err(e) = queued {
                log::debug!("export folder not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the folder dialog thread: {e}");
            g.set_error(format!("Could not open the folder dialog: {e}").into());
        }
    }

    fn export_start(&self) {
        let Some(win) = self.win() else { return };
        let g = win.global::<ExportState>();
        if ACTIVE.with(|a| a.borrow().is_some()) {
            return;
        }
        let fail = |text: &str| g.set_error(text.into());
        let dest = PathBuf::from(g.get_dest().as_str().trim());
        if !dest.is_absolute() {
            return fail("Choose a folder to save to");
        }
        if dest.is_file() {
            return fail("That path is a file, not a folder");
        }
        let (range, range_text) = match read_range(g.get_range(), g.get_from_text().as_str(), g.get_to_text().as_str()) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        let (from, to) = match range_bounds(range, Local::now().date_naive(), &Local) {
            Ok(b) => b,
            Err(e) => return fail(&e),
        };
        let p = self.p();
        let Some((chat_jid, chat_name, is_group)) = TARGET.with(|t| {
            t.borrow().as_ref().filter(|t| t.profile == p.id).map(|t| (t.chat_jid.clone(), t.chat_name.clone(), t.is_group))
        }) else {
            return fail("Open the chat again to export it");
        };
        let job = Job {
            chat_jid,
            ctx: ChatCtx { name: chat_name, is_group },
            format: Format::from_index(g.get_format()),
            from,
            to,
            range_text,
            include_media: g.get_include_media(),
            dest,
            media_root: self.profile_dir(&p).join("media"),
        };
        let store = p.store.clone();
        let generation = GENERATION.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let spawned = std::thread::Builder::new().name("chat-export".into()).spawn(move || {
            let mut report = |done: u64, total: u64| {
                let queued = slint::invoke_from_event_loop(move || {
                    with_app(|a| a.export_progress(generation, done, total));
                });
                if let Err(e) = queued {
                    log::debug!("export progress not delivered: {e}");
                }
            };
            let result = export::run(&store, &job, &flag, &mut report);
            let queued = slint::invoke_from_event_loop(move || {
                with_app(|a| a.export_finished(generation, result));
            });
            if let Err(e) = queued {
                log::error!("export result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start the export thread: {e}");
            return fail(&format!("Could not start the export: {e}"));
        }
        ACTIVE.with(|a| *a.borrow_mut() = Some(Active { generation, cancel }));
        g.set_error("".into());
        g.set_progress(0.0);
        g.set_progress_text("Starting…".into());
        g.set_running(true);
    }

    fn export_progress(&self, generation: u64, done: u64, total: u64) {
        if ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.generation)) != Some(generation) {
            return;
        }
        let Some(win) = self.win() else { return };
        let g = win.global::<ExportState>();
        if g.get_progress_text().as_str() == "Cancelling…" {
            return;
        }
        g.set_progress(if total == 0 { 0.0 } else { done as f32 / total as f32 });
        g.set_progress_text(format!("{} / {} messages", group_digits(done), group_digits(total)).into());
    }

    fn export_finished(&self, generation: u64, result: Result<Outcome, String>) {
        let current = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.generation));
        if current != Some(generation) {
            return;
        }
        ACTIVE.with(|a| a.borrow_mut().take());
        let Some(win) = self.win() else { return };
        let g = win.global::<ExportState>();
        g.set_running(false);
        match result {
            Ok(Outcome::Done { messages, media_copied, media_missing, dest }) => {
                // The export may have used "<name> (2)"; show and open the real folder.
                g.set_dest(dest.display().to_string().into());
                g.set_result_text(result_text(messages, media_copied, media_missing, &dest).into());
                g.set_finished(true);
            }
            Ok(Outcome::Cancelled) => g.set_error("Export cancelled".into()),
            Err(e) => {
                log::warn!("chat export failed");
                log::debug!("chat export error: {e}");
                g.set_error(e.into());
            }
        }
    }

    /// Cancel button: stops the worker; the dialog returns to the form once it
    /// has cleaned up.
    fn export_cancel(&self) {
        let Some(flag) = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.cancel.clone())) else { return };
        flag.store(true, Ordering::Relaxed);
        if let Some(win) = self.win() {
            win.global::<ExportState>().set_progress_text("Cancelling…".into());
        }
    }

    /// Closes the dialog; a running export is cancelled first.
    pub(super) fn export_close(&self) {
        if let Some(a) = ACTIVE.with(|a| a.borrow_mut().take()) {
            a.cancel.store(true, Ordering::Relaxed);
        }
        TARGET.with(|t| *t.borrow_mut() = None);
        if let Some(win) = self.win() {
            let g = win.global::<ExportState>();
            g.set_running(false);
            g.set_finished(false);
            g.set_open(false);
            win.invoke_focus_composer();
        }
    }

    /// The open chat or profile changed: the dialog belongs to the old one.
    pub(super) fn export_chat_changed(&self) {
        if TARGET.with(|t| t.borrow().is_some()) {
            self.export_close();
        }
    }

    fn export_open_folder(&self) {
        let Some(win) = self.win() else { return };
        let dest = PathBuf::from(win.global::<ExportState>().get_dest().as_str().trim());
        if dest.is_dir() {
            run_launcher(dest, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn default_folder_is_named_after_chat_and_day() {
        let dest = default_dest(Some(PathBuf::from("C:\\Users\\x\\Documents")), "Ann: Bob?", date(2026, 3, 7));
        assert_eq!(
            dest,
            Some(PathBuf::from("C:\\Users\\x\\Documents").join("WaClient Export").join("Ann Bob 2026-03-07"))
        );
        assert_eq!(default_dest(None, "Ann", date(2026, 3, 7)), None);
    }

    #[test]
    fn the_range_choice_maps_to_a_range() {
        assert_eq!(read_range(0, "", "").map(|r| r.0), Ok(Range::All));
        assert_eq!(read_range(1, "", "").map(|r| r.0), Ok(Range::Last30Days));
        let (r, text) = read_range(2, "2026-01-02", "2026/02/03").expect("range");
        assert_eq!(r, Range::Custom { from: date(2026, 1, 2), to: date(2026, 2, 3) });
        assert_eq!(text, "2026-01-02 to 2026-02-03");
        assert!(read_range(2, "2026-01-02", "").is_err());
        assert!(read_range(2, "soon", "2026-01-01").is_err());
    }

    #[test]
    fn the_result_names_counts_and_folder() {
        let text = result_text(5_800, 12, 3, Path::new("C:\\out"));
        assert_eq!(text, "Exported 5,800 messages, 12 media files copied, 3 not downloaded.\nC:\\out");
        assert_eq!(result_text(1, 0, 0, Path::new("D")), "Exported 1 message.\nD");
    }
}
