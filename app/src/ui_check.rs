//! Compiles the standalone emoji/reaction .slint files so syntax and type
//! errors surface in `cargo test` even before they are wired into the app.

use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

// The interpreter only lists the last exported component of a file when it does
// not inherit Window, but every component in the file is still type-checked.
fn assert_clean(file: &str, expected: &[&str]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join(file);
    let result = block_on(slint_interpreter::Compiler::default().build_from_path(path));
    let diags: Vec<String> = result
        .diagnostics()
        .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
        .map(|d| d.to_string())
        .collect();
    assert!(diags.is_empty(), "{file} errors:\n{}", diags.join("\n"));
    let names: Vec<&str> = result.component_names().collect();
    for want in expected {
        assert!(names.contains(want), "{file} does not export {want}: {names:?}");
    }
}

#[test]
fn emoji_picker_slint_compiles() {
    assert_clean("emoji_picker.slint", &["EmojiPicker"]);
}

#[test]
fn reactions_slint_compiles() {
    assert_clean("reactions.slint", &["QuickReactPopup"]);
}

#[test]
fn msg_menu_slint_compiles() {
    assert_clean("msg_menu.slint", &["Toast"]);
}

#[test]
fn attach_preview_slint_compiles() {
    assert_clean("attach_preview.slint", &["DropOverlay"]);
}

#[test]
fn chat_search_slint_compiles() {
    assert_clean("chat_search.slint", &["SearchResultList"]);
}
