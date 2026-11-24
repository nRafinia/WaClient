//! Emoji picker data (built once), recents persistence and reactions.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use wa_engine::Command;

use super::App;
use crate::emoji::{self, QUICK_REACTIONS, Recents};
use crate::{EmojiRow, EmojiTab, MainWindow, ReactionConfig};

/// Emoji per row of the picker grid.
const PER_ROW: usize = 8;

pub(super) struct EmojiState {
    rows: Rc<VecModel<EmojiRow>>,
    tabs: Rc<VecModel<EmojiTab>>,
    recents_model: Rc<VecModel<SharedString>>,
    recents: RefCell<Recents>,
}

fn chunk_rows<'a>(items: impl Iterator<Item = &'a str>) -> Vec<EmojiRow> {
    let items: Vec<&str> = items.collect();
    items
        .chunks(PER_ROW)
        .map(|chunk| EmojiRow {
            header: SharedString::default(),
            cells: ModelRc::new(VecModel::from(chunk.iter().map(|e| SharedString::from(*e)).collect::<Vec<_>>())),
        })
        .collect()
}

/// A header row then the emoji rows of each category; a tab per category
/// pointing at its header row.
pub(super) fn category_rows() -> (Vec<EmojiRow>, Vec<EmojiTab>) {
    let mut rows = Vec::new();
    let mut tabs = Vec::new();
    for cat in emoji::categories() {
        tabs.push(EmojiTab {
            icon: cat.icon.into(),
            row: i32::try_from(rows.len()).unwrap_or(0),
        });
        rows.push(EmojiRow { header: cat.name.into(), cells: ModelRc::default() });
        rows.extend(chunk_rows(cat.emoji.iter().map(|e| e.ch)));
    }
    (rows, tabs)
}

/// The picker's rows for a search box content (categories when it is empty).
pub(super) fn picker_rows(query: &str) -> Vec<EmojiRow> {
    if query.trim().is_empty() {
        category_rows().0
    } else {
        chunk_rows(emoji::search(query).into_iter().map(|e| e.ch))
    }
}

impl EmojiState {
    /// `recents_path` is `None` in the demo (nothing is written).
    pub(super) fn new(recents_path: Option<PathBuf>) -> Self {
        let (rows, tabs) = category_rows();
        let recents = recents_path.map(Recents::load).unwrap_or_default();
        let model: Vec<SharedString> = recents.items().iter().map(|e| e.as_str().into()).collect();
        EmojiState {
            rows: Rc::new(VecModel::from(rows)),
            tabs: Rc::new(VecModel::from(tabs)),
            recents_model: Rc::new(VecModel::from(model)),
            recents: RefCell::new(recents),
        }
    }

    pub(super) fn attach(&self, win: &MainWindow) {
        win.set_emoji_rows(ModelRc::from(self.rows.clone()));
        win.set_emoji_tabs(ModelRc::from(self.tabs.clone()));
        win.set_emoji_recents(ModelRc::from(self.recents_model.clone()));
        let quick: Vec<SharedString> = QUICK_REACTIONS.iter().map(|e| SharedString::from(*e)).collect();
        win.global::<ReactionConfig>().set_quick(ModelRc::new(VecModel::from(quick)));
    }
}

impl App {
    /// An empty query shows the categories again.
    pub(super) fn emoji_search(&self, query: &str) {
        self.emoji.rows.set_vec(picker_rows(query));
    }

    /// Remembers a picked emoji (most recent first) for the "Recent" row.
    pub(super) fn emoji_picked(&self, e: &str) {
        let saved = {
            let mut recents = self.emoji.recents.borrow_mut();
            recents.push(e);
            self.emoji
                .recents_model
                .set_vec(recents.items().iter().map(|e| e.as_str().into()).collect::<Vec<SharedString>>());
            recents.save()
        };
        if let Err(err) = saved {
            log::warn!("could not save the recent emoji: {err}");
        }
    }

    /// Inserts at the composer's cursor; the picker stays open.
    pub(super) fn emoji_insert(&self, e: &str) {
        // Also the path of typing redirected from a selected message text.
        if !crate::format::insertable_text(e) {
            return;
        }
        let Some(win) = self.win() else { return };
        let text = win.invoke_composer_text().to_string();
        let mut at = usize::try_from(win.invoke_composer_cursor()).unwrap_or(text.len()).min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        let new = format!("{}{e}{}", &text[..at], &text[at..]);
        let cursor = i32::try_from(at + e.len()).unwrap_or(i32::MAX);
        win.invoke_set_composer(new.as_str().into(), cursor);
        self.on_edit(&new);
    }

    /// Reacts to a message, or removes our reaction when the same emoji is
    /// picked again. The engine stores it right away and reports a change.
    pub(super) fn react(&self, id: &str, emoji: &str) {
        if emoji.is_empty() {
            return;
        }
        let p = self.p();
        let (chat_jid, remove) = {
            let st = p.st.borrow();
            let Some(chat) = st.selected.clone() else { return };
            if !st.msgs.iter().any(|m| m.id == id && !m.revoked) {
                return;
            }
            let mine = st.reactions.get(id).is_some_and(|c| c.iter().any(|c| c.mine && c.emoji == emoji));
            (chat, mine)
        };
        // Optimistic: a second tap before the engine reports back must toggle
        // against what this tap did, not against the old state.
        {
            let mut st = p.st.borrow_mut();
            let chips = st.reactions.entry(id.to_string()).or_default();
            if remove {
                if let Some(i) = chips.iter().position(|c| c.mine && c.emoji == emoji) {
                    chips[i].mine = false;
                    chips[i].count = chips[i].count.saturating_sub(1);
                    if chips[i].count == 0 {
                        chips.remove(i);
                    }
                }
            } else {
                // One reaction per user: the previous one is replaced.
                if let Some(i) = chips.iter().position(|c| c.mine) {
                    chips[i].mine = false;
                    chips[i].count = chips[i].count.saturating_sub(1);
                    if chips[i].count == 0 {
                        chips.remove(i);
                    }
                }
                match chips.iter_mut().find(|c| c.emoji == emoji) {
                    Some(c) => {
                        c.count += 1;
                        c.mine = true;
                    }
                    None => chips.push(crate::emoji::ReactionChip { emoji: emoji.to_string(), count: 1, mine: true }),
                }
            }
        }
        let wire = if remove { String::new() } else { emoji.to_string() };
        self.send(Command::SendReaction { chat_jid, id: id.to_string(), emoji: wire });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn tabs_point_at_header_rows() {
        let (rows, tabs) = category_rows();
        assert_eq!(tabs.len(), emoji::categories().len());
        for (tab, cat) in tabs.iter().zip(emoji::categories()) {
            let row = &rows[usize::try_from(tab.row).expect("row")];
            assert_eq!(row.header.as_str(), cat.name);
        }
        assert!(rows.iter().filter(|r| r.header.is_empty()).all(|r| r.cells.row_count() <= PER_ROW));
    }

    #[test]
    fn search_rows_have_no_headers() {
        let rows = chunk_rows(emoji::search("heart").into_iter().map(|e| e.ch));
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.header.is_empty()));
    }
}
