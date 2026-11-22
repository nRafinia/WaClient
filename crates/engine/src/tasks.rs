//! Background tasks of one bot generation.
//!
//! Tasks that hold an `Arc<Client>` must not outlive their generation: a
//! leftover one keeps the old client (and its session database) alive, which
//! makes wiping the session fail on Windows. They are spawned through
//! [`GenTasks`], which aborts and awaits them when the generation ends.

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::media::lock;

/// How long the supervisor waits for aborted tasks to finish.
pub const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct GenTasks {
    cancelled: AtomicBool,
    set: Mutex<JoinSet<()>>,
}

impl GenTasks {
    /// Spawns `fut` unless the generation is already over (then it is dropped).
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut set = lock(&self.set);
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        // Reap finished tasks so the set does not grow for the whole session.
        while set.try_join_next().is_some() {}
        set.spawn(fut);
    }

    /// `true` once the generation is ending; blocking work polls this between
    /// units, since it cannot be aborted.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Cancels, aborts and awaits every task (bounded by `timeout`).
    pub async fn shutdown(&self, timeout: Duration) {
        let mut set = {
            let mut guard = lock(&self.set);
            self.cancelled.store(true, Ordering::Release);
            std::mem::take(&mut *guard)
        };
        set.abort_all();
        let drained = tokio::time::timeout(timeout, async {
            while set.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            log::warn!("generation tasks did not stop within {timeout:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn shutdown_aborts_running_tasks_and_rejects_new_ones() {
        let tasks = GenTasks::default();
        let guard = Arc::new(());
        let held = guard.clone();
        tasks.spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        assert_eq!(Arc::strong_count(&guard), 2);

        tasks.shutdown(Duration::from_secs(3)).await;
        assert_eq!(Arc::strong_count(&guard), 1, "task future was dropped");
        assert!(tasks.is_cancelled());

        let held = guard.clone();
        tasks.spawn(async move {
            let _held = held;
        });
        assert_eq!(Arc::strong_count(&guard), 1, "spawn after shutdown is a no-op");
    }
}
