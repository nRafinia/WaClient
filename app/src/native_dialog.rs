//! Native message boxes for failures that happen before (or without) the main
//! window, which would otherwise be silent in the windows-subsystem build.

use std::path::Path;

/// What the user chose when the configured data folder is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootChoice {
    Retry,
    /// Run this session from the default folder, changing nothing on disk.
    UseDefault,
    Quit,
}

pub fn root_unavailable_text(path: &Path, reason: &str) -> String {
    format!(
        "WaClient cannot use its data folder:\n\n{}\n\n({reason})\n\n\
         If it is on an external or network drive, connect it first.\n\n\
         Retry: try again\n\
         Ignore: use the default folder for this session only (your data is not touched)\n\
         Abort: quit WaClient",
        path.display()
    )
}

pub fn location_unreadable_text(path: &Path, reason: &str) -> String {
    format!(
        "location.json could not be read:\n\n{}\n\n({reason})\n\n\
         It tells WaClient where your data is stored; it was left untouched.\n\n\
         Retry: try again\n\
         Ignore: use a temporary empty session (your data is not touched)\n\
         Abort: quit WaClient",
        path.display()
    )
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use super::RootChoice;

    const MB_OK: u32 = 0x0;
    const MB_ABORTRETRYIGNORE: u32 = 0x2;
    const MB_ICONERROR: u32 = 0x10;
    const MB_ICONWARNING: u32 = 0x30;
    const MB_SETFOREGROUND: u32 = 0x1_0000;
    const MB_TOPMOST: u32 = 0x4_0000;
    const IDRETRY: i32 = 4;
    const IDIGNORE: i32 = 5;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn show(title: &str, text: &str, kind: u32) -> i32 {
        let (text, title) = (wide(text), wide(title));
        // SAFETY: both strings are NUL-terminated and outlive the call; a null
        // owner window is allowed.
        unsafe { MessageBoxW(0, text.as_ptr(), title.as_ptr(), kind | MB_SETFOREGROUND | MB_TOPMOST) }
    }

    pub fn root_unavailable(path: &Path, reason: &str) -> RootChoice {
        let text = super::root_unavailable_text(path, reason);
        match show("WaClient: data folder unavailable", &text, MB_ABORTRETRYIGNORE | MB_ICONWARNING) {
            IDRETRY => RootChoice::Retry,
            IDIGNORE => RootChoice::UseDefault,
            _ => RootChoice::Quit,
        }
    }

    pub fn location_unreadable(path: &Path, reason: &str) -> RootChoice {
        let text = super::location_unreadable_text(path, reason);
        match show("WaClient: location.json unreadable", &text, MB_ABORTRETRYIGNORE | MB_ICONWARNING) {
            IDRETRY => RootChoice::Retry,
            IDIGNORE => RootChoice::UseDefault,
            _ => RootChoice::Quit,
        }
    }

    pub fn fatal(title: &str, text: &str) {
        show(title, text, MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use super::RootChoice;

    pub fn root_unavailable(path: &Path, reason: &str) -> RootChoice {
        log::error!("{}", super::root_unavailable_text(path, reason));
        RootChoice::Quit
    }

    pub fn location_unreadable(path: &Path, reason: &str) -> RootChoice {
        log::error!("{}", super::location_unreadable_text(path, reason));
        RootChoice::Quit
    }

    pub fn fatal(title: &str, text: &str) {
        log::error!("{title}: {text}");
    }
}

pub use imp::{fatal, location_unreadable, root_unavailable};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_the_folder_and_all_three_choices() {
        let text = root_unavailable_text(Path::new(r"E:\data\WaClient"), "gone");
        assert!(text.contains(r"E:\data\WaClient") && text.contains("gone"));
        assert!(text.contains("Retry") && text.contains("Ignore") && text.contains("Abort"));
    }
}
