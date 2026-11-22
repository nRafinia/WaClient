//! Persistent user settings: per-profile `settings.json`, the global
//! "start with Windows" registry value, and the media cache cleanup.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use wa_engine::EngineSettings;

const SETTINGS_FILE: &str = "settings.json";
const MIN_MAX_MB: u32 = 1;
const MAX_MAX_MB: u32 = 100;
pub const FONT_SCALE_MIN: f32 = 0.85;
pub const FONT_SCALE_MAX: f32 = 1.3;
/// Scales behind the Small / Normal / Large choices of the settings page.
pub const FONT_SCALES: [f32; 3] = [0.9, 1.0, 1.15];

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WaClient";
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Serializable mirror of `wa_engine::EngineSettings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettingsDto {
    pub auto_download_images: bool,
    pub auto_download_voice: bool,
    pub auto_download_documents: bool,
    pub auto_download_max_mb: u32,
    pub send_read_receipts: bool,
    pub send_status_receipts: bool,
    pub send_typing: bool,
    pub send_presence: bool,
    pub notifications: bool,
    pub send_link_previews: bool,
}

impl Default for EngineSettingsDto {
    fn default() -> Self {
        let e = EngineSettings::default();
        EngineSettingsDto {
            auto_download_images: e.auto_download_images,
            auto_download_voice: e.auto_download_voice,
            auto_download_documents: e.auto_download_documents,
            auto_download_max_mb: e.auto_download_max_mb,
            send_read_receipts: e.send_read_receipts,
            send_status_receipts: e.send_status_receipts,
            send_typing: e.send_typing,
            send_presence: true,
            notifications: e.notifications,
            send_link_previews: e.send_link_previews,
        }
    }
}

impl EngineSettingsDto {
    pub fn to_engine(&self) -> EngineSettings {
        EngineSettings {
            auto_download_images: self.auto_download_images,
            auto_download_voice: self.auto_download_voice,
            auto_download_documents: self.auto_download_documents,
            auto_download_max_mb: self.auto_download_max_mb,
            send_read_receipts: self.send_read_receipts,
            send_status_receipts: self.send_status_receipts,
            send_typing: self.send_typing,
            send_presence: self.send_presence,
            notifications: self.notifications,
            send_link_previews: self.send_link_previews,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn from_engine(e: &EngineSettings) -> Self {
        EngineSettingsDto {
            auto_download_images: e.auto_download_images,
            auto_download_voice: e.auto_download_voice,
            auto_download_documents: e.auto_download_documents,
            auto_download_max_mb: e.auto_download_max_mb,
            send_read_receipts: e.send_read_receipts,
            send_status_receipts: e.send_status_receipts,
            send_typing: e.send_typing,
            send_presence: e.send_presence,
            notifications: e.notifications,
            send_link_previews: e.send_link_previews,
        }
    }
}

/// The audience last chosen for a status post, remembered per profile.
/// `kind`: 0 my contacts, 1 my contacts except `jids`, 2 only `jids`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusAudiencePref {
    pub kind: u8,
    pub jids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub engine: EngineSettingsDto,
    pub close_to_tray: bool,
    /// Mirrors the global autostart registry value; not authoritative on disk,
    /// the caller syncs it through [`set_autostart`].
    pub start_minimized_with_windows: bool,
    /// Show the message text in toasts (otherwise "New message").
    pub notification_preview: bool,
    pub notification_sound: bool,
    pub enter_sends: bool,
    pub font_scale: f32,
    pub status_audience: StatusAudiencePref,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            engine: EngineSettingsDto::default(),
            close_to_tray: true,
            start_minimized_with_windows: false,
            notification_preview: true,
            notification_sound: true,
            enter_sends: true,
            font_scale: 1.0,
            status_audience: StatusAudiencePref::default(),
        }
    }
}

impl AppSettings {
    /// Forces every value into its allowed range.
    pub fn clamped(mut self) -> Self {
        self.engine.auto_download_max_mb =
            self.engine.auto_download_max_mb.clamp(MIN_MAX_MB, MAX_MAX_MB);
        self.font_scale = if self.font_scale.is_finite() {
            self.font_scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
        } else {
            1.0
        };
        self
    }

    /// Index into [`FONT_SCALES`] closest to the current scale.
    pub fn font_scale_index(&self) -> usize {
        let mut best = 0;
        for (i, s) in FONT_SCALES.iter().enumerate() {
            if (s - self.font_scale).abs() < (FONT_SCALES[best] - self.font_scale).abs() {
                best = i;
            }
        }
        best
    }

    pub fn set_font_scale_index(&mut self, index: usize) {
        self.font_scale = FONT_SCALES[index.min(FONT_SCALES.len() - 1)];
    }
}

fn settings_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

/// Loads `<dir>/settings.json`. A missing file yields defaults; a corrupt one
/// is moved aside to `settings.json.bad` and defaults are returned.
pub fn load(dir: &Path) -> AppSettings {
    let path = settings_path(dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("reading {}: {e}", path.display());
            }
            return AppSettings::default();
        }
    };
    match serde_json::from_slice::<AppSettings>(&bytes) {
        Ok(s) => s.clamped(),
        Err(e) => {
            log::warn!("corrupt {}: {e}; using defaults", path.display());
            let bad = dir.join(format!("{SETTINGS_FILE}.bad"));
            if let Err(e) = fs::rename(&path, &bad) {
                log::warn!("backing up {}: {e}", path.display());
            }
            AppSettings::default()
        }
    }
}

/// Atomically writes `<dir>/settings.json` (temp file + rename).
pub fn save(dir: &Path, settings: &AppSettings) -> anyhow::Result<()> {
    fs::create_dir_all(dir).context("creating settings dir")?;
    let json = serde_json::to_vec_pretty(&settings.clone().clamped())
        .context("serializing settings")?;
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).context("creating temp settings file")?;
        f.write_all(&json).context("writing temp settings file")?;
        f.sync_all().context("flushing temp settings file")?;
    }
    fs::rename(&tmp, settings_path(dir)).context("replacing settings file")?;
    Ok(())
}

/// The command line stored in the Run value; identical to the installer's.
pub fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" {MINIMIZED_FLAG}", exe.display())
}

#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

#[cfg(not(windows))]
pub fn autostart_enabled() -> bool {
    false
}

#[cfg(windows)]
pub fn set_autostart(enabled: bool, exe: &Path) -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if enabled {
        let (key, _) = hkcu.create_subkey(RUN_KEY).context("opening Run key")?;
        key.set_value(RUN_VALUE, &autostart_command(exe))
            .context("writing autostart value")?;
    } else {
        let key = hkcu
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .context("opening Run key")?;
        match key.delete_value(RUN_VALUE) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("removing autostart value"),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_autostart(_enabled: bool, _exe: &Path) -> anyhow::Result<()> {
    anyhow::bail!("autostart is only supported on Windows")
}

/// Deletes downloaded media under `<profile_dir>/media` and returns the bytes
/// freed. `avatars/` (profile and chat pictures) and `outgoing/` (files staged
/// for sending) are kept, and nothing outside `media/` is touched.
pub fn clear_media_cache(profile_dir: &Path) -> anyhow::Result<u64> {
    let media = profile_dir.join("media");
    let entries = match fs::read_dir(&media) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).context("reading media dir"),
    };
    let mut freed = 0;
    for entry in entries {
        let entry = entry.context("reading media entry")?;
        let name = entry.file_name();
        if name == "avatars" || name == "outgoing" {
            continue;
        }
        freed += remove_tree(&entry.path())?;
    }
    Ok(freed)
}

/// Removes a file or directory tree (without following symlinks); returns bytes freed.
fn remove_tree(path: &Path) -> anyhow::Result<u64> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        let mut freed = 0;
        for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            freed += remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path).with_context(|| format!("removing {}", path.display()))?;
        Ok(freed)
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        Ok(meta.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "waclient-settings-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("create temp dir");
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults() {
        let s = AppSettings::default();
        assert!(s.close_to_tray);
        assert!(!s.start_minimized_with_windows);
        assert!(s.notification_preview);
        assert!(s.notification_sound);
        assert!(s.enter_sends);
        assert_eq!(s.font_scale, 1.0);
        assert!(s.engine.send_presence);
        assert_eq!(s.engine.auto_download_max_mb, 16);
        assert_eq!(s.font_scale_index(), 1);
    }

    #[test]
    fn roundtrip() {
        let t = TempDir::new();
        let mut s = AppSettings::default();
        s.close_to_tray = false;
        s.enter_sends = false;
        s.font_scale = 1.15;
        s.engine.send_presence = false;
        s.engine.auto_download_max_mb = 42;
        save(&t.0, &s).expect("save");
        assert!(!t.0.join("settings.json.tmp").exists());
        assert_eq!(load(&t.0), s);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let t = TempDir::new();
        assert_eq!(load(&t.0), AppSettings::default());
    }

    #[test]
    fn partial_json_fills_defaults() {
        let t = TempDir::new();
        fs::write(
            t.0.join("settings.json"),
            r#"{"close_to_tray":false,"engine":{"send_typing":false}}"#,
        )
        .expect("write");
        let s = load(&t.0);
        assert!(!s.close_to_tray);
        assert!(!s.engine.send_typing);
        assert!(s.engine.send_read_receipts);
        assert!(s.engine.send_presence);
        assert!(s.notification_sound);
    }

    #[test]
    fn corrupt_file_gives_defaults_and_backup() {
        let t = TempDir::new();
        fs::write(t.0.join("settings.json"), b"{ not json").expect("write");
        assert_eq!(load(&t.0), AppSettings::default());
        assert!(!t.0.join("settings.json").exists());
        assert_eq!(fs::read(t.0.join("settings.json.bad")).expect("bak"), b"{ not json");
    }

    #[test]
    fn clamping() {
        let mut s = AppSettings::default();
        s.engine.auto_download_max_mb = 0;
        s.font_scale = 5.0;
        let c = s.clone().clamped();
        assert_eq!(c.engine.auto_download_max_mb, 1);
        assert_eq!(c.font_scale, FONT_SCALE_MAX);

        s.engine.auto_download_max_mb = 9999;
        s.font_scale = 0.1;
        let c = s.clone().clamped();
        assert_eq!(c.engine.auto_download_max_mb, 100);
        assert_eq!(c.font_scale, FONT_SCALE_MIN);

        s.font_scale = f32::NAN;
        assert_eq!(s.clamped().font_scale, 1.0);
    }

    #[test]
    fn load_and_save_clamp() {
        let t = TempDir::new();
        fs::write(
            t.0.join("settings.json"),
            r#"{"font_scale":9.0,"engine":{"auto_download_max_mb":500}}"#,
        )
        .expect("write");
        let s = load(&t.0);
        assert_eq!(s.font_scale, FONT_SCALE_MAX);
        assert_eq!(s.engine.auto_download_max_mb, 100);
    }

    #[test]
    fn engine_conversion() {
        let mut dto = EngineSettingsDto::default();
        assert_eq!(dto.to_engine().auto_download_max_mb, EngineSettings::default().auto_download_max_mb);
        assert!(dto.to_engine().send_presence);

        dto.auto_download_images = false;
        dto.auto_download_documents = true;
        dto.auto_download_max_mb = 7;
        dto.send_read_receipts = false;
        dto.send_status_receipts = false;
        dto.send_typing = false;
        dto.send_presence = false;
        dto.notifications = false;
        dto.send_link_previews = true;
        let e = dto.to_engine();
        assert!(!e.auto_download_images);
        assert!(e.auto_download_documents);
        assert_eq!(e.auto_download_max_mb, 7);
        assert!(!e.send_read_receipts);
        assert!(!e.send_status_receipts);
        assert!(!e.send_typing);
        assert!(!e.send_presence);
        assert!(!e.notifications);
        assert!(e.send_link_previews);
        assert_eq!(EngineSettingsDto::from_engine(&e), dto);
    }

    #[test]
    fn font_scale_choices() {
        let mut s = AppSettings::default();
        for i in 0..3 {
            s.set_font_scale_index(i);
            assert_eq!(s.font_scale_index(), i);
        }
        s.set_font_scale_index(99);
        assert_eq!(s.font_scale_index(), 2);
    }

    #[test]
    fn autostart_command_matches_installer() {
        let cmd = autostart_command(Path::new(r"C:\Program Files\WaClient\WaClient.exe"));
        assert_eq!(cmd, r#""C:\Program Files\WaClient\WaClient.exe" --minimized"#);
    }

    #[test]
    fn cache_clear_keeps_avatars_and_db() {
        let t = TempDir::new();
        let media = t.0.join("media");
        fs::create_dir_all(media.join("avatars")).expect("dir");
        fs::create_dir_all(media.join("outgoing")).expect("dir");
        fs::create_dir_all(media.join("chat1")).expect("dir");
        fs::write(media.join("avatars").join("a.jpg"), b"avatar").expect("w");
        fs::write(media.join("outgoing").join("o.bin"), b"out").expect("w");
        fs::write(media.join("pic.jpg"), b"12345").expect("w");
        fs::write(media.join("chat1").join("doc.pdf"), b"1234567890").expect("w");
        fs::write(t.0.join("store.db"), b"db").expect("w");

        assert_eq!(clear_media_cache(&t.0).expect("clear"), 15);
        assert!(!media.join("pic.jpg").exists());
        assert!(!media.join("chat1").exists());
        assert!(media.join("avatars").join("a.jpg").exists());
        assert!(media.join("outgoing").join("o.bin").exists());
        assert!(t.0.join("store.db").exists());
        assert_eq!(clear_media_cache(&t.0).expect("again"), 0);
    }

    #[test]
    fn cache_clear_without_media_dir() {
        let t = TempDir::new();
        assert_eq!(clear_media_cache(&t.0).expect("clear"), 0);
    }

    #[test]
    fn settings_slint_compiles() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join("settings.slint");
        let source = fs::read_to_string(&path).expect("read settings.slint");
        let compiler = slint_interpreter::Compiler::default();
        let result = spin_on(compiler.build_from_source(source, path));
        let errors: Vec<String> = result
            .diagnostics()
            .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
            .map(|d| d.to_string())
            .collect();
        assert!(errors.is_empty(), "slint errors: {errors:#?}");
        assert!(result.component("SettingsPage").is_some());
    }

    /// Minimal executor: `build_from_source` never actually suspends for file sources.
    fn spin_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }
}
