//! Bounded worker pool for image decoding (chat images and avatars), so a
//! chat full of pictures cannot spawn a thread per image.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::models;

pub enum DecodeJob {
    /// A full-size in-chat image; only useful while `chat_jid` is open.
    Full { chat_jid: String, key: String, msg_id: String, path: PathBuf, max_dim: u32 },
    Avatar { path: PathBuf, max_dim: u32 },
    /// A status image; results are never stale-skipped (the viewer checks them).
    Media { path: PathBuf, max_dim: u32 },
}

pub enum DecodeDone {
    Full {
        chat_jid: String,
        key: String,
        msg_id: String,
        buf: Option<SharedPixelBuffer<Rgba8Pixel>>,
    },
    Avatar { path: String, buf: Option<SharedPixelBuffer<Rgba8Pixel>> },
    Media { path: String, buf: Option<SharedPixelBuffer<Rgba8Pixel>> },
}

/// True when a result for `job_chat` is no longer wanted because another
/// chat (or none) is open.
pub fn is_stale(open_chat: Option<&str>, job_chat: &str) -> bool {
    open_chat != Some(job_chat)
}

type OpenChat = Arc<Mutex<Option<String>>>;

pub struct DecodePool {
    tx: Sender<DecodeJob>,
    open_chat: OpenChat,
}

impl DecodePool {
    /// Starts `threads` workers sharing one queue. `on_done` runs on a worker
    /// thread for every finished job.
    pub fn new(threads: usize, on_done: impl Fn(DecodeDone) + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<DecodeJob>();
        let rx = Arc::new(Mutex::new(rx));
        let on_done = Arc::new(on_done);
        let open_chat: OpenChat = Arc::new(Mutex::new(None));
        for n in 0..threads {
            let rx = rx.clone();
            let on_done = on_done.clone();
            let open_chat = open_chat.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("image-decode-{n}"))
                .spawn(move || worker(&rx, &open_chat, &*on_done));
            if let Err(e) = spawned {
                log::error!("could not start an image decoder thread: {e}");
            }
        }
        Self { tx, open_chat }
    }

    /// Queues a job; false when no worker is left to run it.
    pub fn submit(&self, job: DecodeJob) -> bool {
        match self.tx.send(job) {
            Ok(()) => true,
            Err(_) => {
                log::error!("the image decoder pool is not running");
                false
            }
        }
    }

    /// Tells the workers which chat is open so queued jobs for other chats
    /// are skipped without decoding.
    pub fn set_open_chat(&self, jid: Option<&str>) {
        match self.open_chat.lock() {
            Ok(mut slot) => *slot = jid.map(str::to_string),
            Err(e) => log::warn!("decoder open-chat state is poisoned: {e}"),
        }
    }
}

fn worker(rx: &Mutex<Receiver<DecodeJob>>, open_chat: &Mutex<Option<String>>, on_done: &dyn Fn(DecodeDone)) {
    loop {
        // The guard is a temporary: it is released before the job runs.
        let job = match rx.lock() {
            Ok(guard) => guard.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { return };
        match job {
            DecodeJob::Full { chat_jid, key, msg_id, path, max_dim } => {
                let stale = open_chat
                    .lock()
                    .map(|open| is_stale(open.as_deref(), &chat_jid))
                    .unwrap_or(false);
                if stale {
                    continue;
                }
                let buf = decode(&path, max_dim);
                on_done(DecodeDone::Full { chat_jid, key, msg_id, buf });
            }
            DecodeJob::Avatar { path, max_dim } => {
                let buf = decode(&path, max_dim);
                on_done(DecodeDone::Avatar { path: path.to_string_lossy().into_owned(), buf });
            }
            DecodeJob::Media { path, max_dim } => {
                let buf = decode(&path, max_dim);
                on_done(DecodeDone::Media { path: path.to_string_lossy().into_owned(), buf });
            }
        }
    }
}

fn decode(path: &std::path::Path, max_dim: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    match models::decode_file_scaled(path, max_dim) {
        Ok(b) => Some(b),
        Err(e) => {
            log::warn!("could not decode {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stale_results_are_detected() {
        assert!(!is_stale(Some("a"), "a"));
        assert!(is_stale(Some("b"), "a"));
        assert!(is_stale(None, "a"));
    }

    #[test]
    fn pool_skips_jobs_for_other_chats() {
        let dir = std::env::temp_dir().join(format!("waclient-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let img = dir.join("a.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])).save(&img).expect("png");

        let (done_tx, done_rx) = mpsc::channel();
        let done_tx = Mutex::new(done_tx);
        // One worker keeps the queue strictly ordered.
        let pool = DecodePool::new(1, move |d| {
            if let Ok(tx) = done_tx.lock() {
                let _ = tx.send(d);
            }
        });
        pool.set_open_chat(Some("open"));
        let full = |chat: &str| DecodeJob::Full {
            chat_jid: chat.into(),
            key: "k".into(),
            msg_id: "m".into(),
            path: img.clone(),
            max_dim: 64,
        };
        assert!(pool.submit(full("other")));
        assert!(pool.submit(full("open")));
        assert!(pool.submit(DecodeJob::Avatar { path: img.clone(), max_dim: 2 }));

        match done_rx.recv_timeout(Duration::from_secs(5)).expect("first result") {
            DecodeDone::Full { chat_jid, buf, .. } => {
                assert_eq!(chat_jid, "open");
                assert!(buf.is_some());
            }
            DecodeDone::Avatar { .. } | DecodeDone::Media { .. } => panic!("the stale job was not skipped"),
        }
        match done_rx.recv_timeout(Duration::from_secs(5)).expect("second result") {
            DecodeDone::Avatar { buf, .. } => {
                let buf = buf.expect("avatar decoded");
                assert!(buf.width() <= 2 && buf.height() <= 2);
            }
            DecodeDone::Full { .. } | DecodeDone::Media { .. } => panic!("unexpected result"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
