//! Deleting the data of removed profiles once nothing in this process can
//! still hold it open.
//!
//! A removed profile's folder is queued in the registry (`pending_delete`, so a
//! crash or a locked file is retried at the next start). Here the UI waits until
//! the profile's UI state has really been dropped (its `Arc<Store>` clone also
//! locks `messages.db` on Windows) and then sweeps the queue.

use std::path::Path;
use std::rc::Weak;
use std::time::Instant;

use crate::profiles::{self, Profile, Registry};

/// One removed profile whose files are still to be deleted.
pub struct DeleteJob<T> {
    pub profile: Profile,
    /// The UI state that owned the profile's store handle. Deleting starts only
    /// once this can no longer be upgraded, i.e. every clone of it is gone.
    pub holder: Weak<T>,
    pub deadline: Instant,
}

/// Runs one attempt for every job and returns the profiles given up on (their
/// folders stay queued in the registry for the next start).
///
/// Nothing is deleted while any job's holder is alive: the sweep covers the
/// whole queue, and a folder must not be deleted under a store that is still open.
pub fn step<T>(registry: &mut Registry, root: &Path, jobs: &mut Vec<DeleteJob<T>>, now: Instant) -> Vec<Profile> {
    // Pending drafts must never be flushed into a folder that is about to go.
    for job in jobs.iter() {
        super::composer::forget_profile(&job.profile.id);
    }
    let all_released = jobs.iter().all(|j| j.holder.strong_count() == 0);
    let failed = all_released.then(|| registry.sweep_pending_deletes(root));
    let mut gave_up = Vec::new();
    jobs.retain(|job| {
        let released = job.holder.strong_count() == 0;
        let done = released
            && if job.profile.in_root {
                match profiles::remove_data(root, &job.profile) {
                    Ok(()) => true,
                    Err(e) => {
                        log::debug!("files of profile {} still in use: {e}", job.profile.id);
                        false
                    }
                }
            } else {
                failed.as_ref().is_some_and(|f| !f.contains(&job.profile.id))
            };
        if done {
            return false;
        }
        if now >= job.deadline {
            log::error!("could not delete the data of profile {} yet", job.profile.id);
            gave_up.push(job.profile.clone());
            return false;
        }
        true
    });
    gave_up
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waclient-removal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn make_profile(reg: &mut Registry, root: &Path, name: &str) -> Profile {
        let p = reg.add_in(root, Some(name), 1).expect("add");
        let dir = profiles::data_dir(root, &p);
        std::fs::create_dir_all(dir.join("media")).expect("dir");
        std::fs::write(dir.join("messages.db"), b"x").expect("file");
        p
    }

    #[test]
    fn folder_is_deleted_only_after_the_holder_is_gone() {
        let root = temp_root("holder");
        let mut reg = Registry::default();
        let keep = make_profile(&mut reg, &root, "Keep");
        let gone = make_profile(&mut reg, &root, "Gone");
        reg.save(&root).expect("save");
        let dir = profiles::data_dir(&root, &gone);

        // Stands in for the ProfileState (and the Store clone it owns).
        let holder = Rc::new(());
        let removed = reg.mark_pending_delete(&root, &gone.id).expect("mark").expect("known");
        let mut jobs =
            vec![DeleteJob { profile: removed, holder: Rc::downgrade(&holder), deadline: Instant::now() + Duration::from_secs(60) }];

        assert!(step(&mut reg, &root, &mut jobs, Instant::now()).is_empty());
        assert!(dir.exists(), "must not delete while the UI state is alive");
        assert_eq!(jobs.len(), 1);

        drop(holder);
        assert!(step(&mut reg, &root, &mut jobs, Instant::now()).is_empty());
        assert!(!dir.exists(), "profile folder must be gone");
        assert!(jobs.is_empty());
        assert!(reg.pending_delete.is_empty());
        assert!(profiles::data_dir(&root, &keep).exists());

        // Settings flushed for a removed profile must not bring it back: the
        // app skips ids that are no longer in `profs` (see App::flush_settings).
        let on_disk = profiles::load_strict(&root).expect("reload");
        assert!(on_disk.get(&gone.id).is_none());
        assert!(on_disk.pending_delete.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gives_up_at_the_deadline_and_keeps_the_delete_queued() {
        let root = temp_root("deadline");
        let mut reg = Registry::default();
        let p = make_profile(&mut reg, &root, "A");
        make_profile(&mut reg, &root, "B");
        reg.save(&root).expect("save");
        let dir = profiles::data_dir(&root, &p);
        let holder = Rc::new(());
        let removed = reg.mark_pending_delete(&root, &p.id).expect("mark").expect("known");
        let now = Instant::now();
        let mut jobs = vec![DeleteJob { profile: removed, holder: Rc::downgrade(&holder), deadline: now }];
        let gave_up = step(&mut reg, &root, &mut jobs, now);
        assert_eq!(gave_up.len(), 1);
        assert!(dir.exists());
        let on_disk = profiles::load_strict(&root).expect("reload");
        assert_eq!(on_disk.pending_delete, vec![p.id.clone()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn root_profile_files_are_removed_without_touching_other_data() {
        let root = temp_root("inroot");
        let mut reg = Registry::default();
        let mut p = reg.add_in(&root, Some("Main"), 1).expect("add");
        p.in_root = true;
        std::fs::write(root.join("messages.db"), b"x").expect("file");
        std::fs::write(root.join("keep.txt"), b"x").expect("file");
        let mut jobs = vec![DeleteJob::<()> { profile: p, holder: Weak::new(), deadline: Instant::now() + Duration::from_secs(60) }];
        assert!(step(&mut reg, &root, &mut jobs, Instant::now()).is_empty());
        assert!(jobs.is_empty());
        assert!(!root.join("messages.db").exists());
        assert!(root.join("keep.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
