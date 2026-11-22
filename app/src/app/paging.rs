//! History paging: the pure rules for loading older messages while the user
//! scrolls up, shared by the main window and the pop-out windows.
//!
//! The list only learns real row heights once rows are laid out, so after older
//! rows are put in front the view is anchored to a *row* (the one that was at the
//! top before), not to a computed pixel offset; see `anchor-begin` in
//! `chat_view.slint`.

use std::time::Duration;

/// How long a load keeps the pager busy: the time the anchored row needs to
/// settle. The next load can only start after it (and only if the view is near
/// the top again).
pub const SETTLE: Duration = Duration::from_millis(600);

/// The view counts as "near the top" below this scroll offset (pixels, see
/// `changed content-y` in `chat_view.slint`).
pub const NEAR_TOP: f32 = 100.0;

/// One load at a time, no matter how many scroll events arrive while rows are
/// being inserted and the view settles.
#[derive(Debug, Default)]
pub struct Pager {
    busy: bool,
}

impl Pager {
    /// A scroll event wants an older page. True when the caller may load now
    /// (and must report back with [`Pager::settled`] or [`Pager::cancel`]).
    pub fn request(&mut self, exhausted: bool) -> bool {
        if exhausted || self.busy {
            return false;
        }
        self.busy = true;
        true
    }

    /// The load gave nothing to wait for (error, empty page, other chat).
    pub fn cancel(&mut self) {
        self.busy = false;
    }

    /// The view has settled after rows were inserted. True when another page is
    /// due right away: the view is still near the top (a short page or a tall
    /// window) and there is more history. Scrolling at the very top produces no
    /// further scroll events, so this is what keeps the loading going.
    pub fn settled(&mut self, scroll_y: f32, exhausted: bool) -> bool {
        self.busy = false;
        scroll_y > -NEAR_TOP && self.request(exhausted)
    }
}

/// Model index of the row to anchor after `rows_after - rows_before` rows were
/// put in front: the row that was first before (the day separator of the oldest
/// message), or, when the new page ends on the same day and that separator is
/// dropped, the message below it.
pub fn anchor_index(rows_before: usize, rows_after: usize, seam_continues_day: bool) -> usize {
    (rows_after + usize::from(seam_continues_day)).saturating_sub(rows_before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_load_runs_at_a_time() {
        let mut p = Pager::default();
        assert!(p.request(false));
        assert!(!p.request(false), "a second request while busy is refused");
        assert!(!p.request(false));
        p.cancel();
        assert!(p.request(false), "free again after a cancelled load");
    }

    #[test]
    fn exhausted_history_never_loads() {
        let mut p = Pager::default();
        assert!(!p.request(true));
        assert!(p.request(false));
        assert!(!p.settled(0.0, true), "no chain once the start of the chat is reached");
        assert!(!p.request(true));
    }

    #[test]
    fn rearms_only_after_settling_and_chains_while_near_top() {
        let mut p = Pager::default();
        assert!(p.request(false));
        assert!(!p.request(false));
        // Settled far from the top: idle, the next scroll event may load again.
        assert!(!p.settled(-2650.0, false));
        assert!(p.request(false));
        // Settled still near the top (short page): the next page is due at once and stays busy.
        assert!(p.settled(-40.0, false));
        assert!(!p.request(false), "the chained load is in flight");
        assert!(!p.settled(-500.0, false));
    }

    #[test]
    fn anchor_is_the_old_first_row() {
        // 50 messages + 2 separators in front, old row 0 now at index 52.
        assert_eq!(anchor_index(10, 62, false), 52);
        // Same day at the seam: the old separator is gone (61 rows), the message sits at 52.
        assert_eq!(anchor_index(10, 61, true), 52);
        assert_eq!(anchor_index(10, 10, false), 0);
    }
}
