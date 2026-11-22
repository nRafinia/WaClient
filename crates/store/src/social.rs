//! Reactions, polls and status updates.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::model::*;
use crate::queries::{message_from_row, MESSAGE_COLS};
use crate::{Result, Store};

/// Ids per `IN (...)` query; stays far below SQLite's variable limit.
const ID_CHUNK: usize = 500;

/// Encodes strings as a JSON array (the poll tables hold option names).
fn encode_json_strings(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in item.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// Inverse of [`encode_json_strings`]; malformed input yields what could be
/// read up to the error (the data is only ever written by us).
fn decode_json_strings(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = json.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut item = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => match chars.next() {
                    Some('n') => item.push('\n'),
                    Some('r') => item.push('\r'),
                    Some('t') => item.push('\t'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        let ch = u32::from_str_radix(&hex, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or('\u{fffd}');
                        item.push(ch);
                    }
                    Some(other) => item.push(other),
                    None => break,
                },
                c => item.push(c),
            }
        }
        if !closed {
            break;
        }
        out.push(item);
    }
    out
}

/// Drops repeated names, keeping the first occurrence of each.
fn dedupe(names: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .iter()
        .filter(|n| seen.insert(n.as_str()))
        .cloned()
        .collect()
}

impl Store {
    /// Records `emoji` as the reaction of `sender_jid` to a message. An empty
    /// emoji removes it. An older reaction never replaces a newer one. The
    /// target message does not need to exist yet.
    pub fn upsert_reaction(
        &self,
        chat_jid: &str,
        msg_id: &str,
        sender_jid: &str,
        emoji: &str,
        ts: i64,
        from_me: bool,
    ) -> Result<()> {
        if emoji.is_empty() {
            return self.remove_reaction(chat_jid, msg_id, sender_jid);
        }
        self.lock().execute(
            "INSERT INTO reactions (chat_jid, msg_id, sender_jid, emoji, ts, from_me) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(chat_jid, msg_id, sender_jid) DO UPDATE SET \
             emoji = excluded.emoji, ts = excluded.ts, from_me = excluded.from_me \
             WHERE excluded.ts >= reactions.ts",
            params![chat_jid, msg_id, sender_jid, emoji, ts, from_me],
        )?;
        Ok(())
    }

    pub fn remove_reaction(&self, chat_jid: &str, msg_id: &str, sender_jid: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM reactions WHERE chat_jid = ?1 AND msg_id = ?2 AND sender_jid = ?3",
            params![chat_jid, msg_id, sender_jid],
        )?;
        Ok(())
    }

    /// The `(emoji, ts)` currently stored for one reactor, if any.
    pub fn reaction_of(
        &self,
        chat_jid: &str,
        msg_id: &str,
        sender_jid: &str,
    ) -> Result<Option<(String, i64)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT emoji, ts FROM reactions \
                 WHERE chat_jid = ?1 AND msg_id = ?2 AND sender_jid = ?3",
                params![chat_jid, msg_id, sender_jid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Reactions per message id as `(emoji, count, mine)`, most used first.
    /// Messages without reactions are absent from the map.
    pub fn reactions_for(
        &self,
        chat_jid: &str,
        msg_ids: &[String],
    ) -> Result<HashMap<String, Vec<(String, u32, bool)>>> {
        let conn = self.lock();
        let mut out: HashMap<String, Vec<(String, u32, bool)>> = HashMap::new();
        for chunk in msg_ids.chunks(ID_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT msg_id, emoji, COUNT(*), MAX(from_me) FROM reactions \
                 WHERE chat_jid = ?1 AND msg_id IN ({marks}) \
                 GROUP BY msg_id, emoji ORDER BY msg_id, COUNT(*) DESC, MIN(ts), emoji"
            );
            let mut stmt = conn.prepare(&sql)?;
            let args = std::iter::once(chat_jid).chain(chunk.iter().map(String::as_str));
            let rows = stmt.query_map(params_from_iter(args), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            })?;
            for row in rows {
                let (id, emoji, count, mine) = row?;
                out.entry(id).or_default().push((emoji, count, mine));
            }
        }
        Ok(out)
    }

    /// Stores a poll definition; an already known poll is left untouched.
    /// Repeated option names are collapsed (votes refer to options by name).
    pub fn upsert_poll(
        &self,
        chat_jid: &str,
        msg_id: &str,
        question: &str,
        options: &[String],
        selectable_count: u32,
    ) -> Result<()> {
        let options = dedupe(options);
        self.lock().execute(
            "INSERT OR IGNORE INTO polls (chat_jid, msg_id, question, options_json, \
             selectable_count) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                chat_jid,
                msg_id,
                question,
                encode_json_strings(&options),
                selectable_count
            ],
        )?;
        Ok(())
    }

    /// Marks `correct` (an option name) as the right answer of a quiz poll;
    /// `None` clears it. Names that are not an option of the poll are ignored.
    pub fn set_poll_correct(&self, chat_jid: &str, msg_id: &str, correct: Option<&str>) -> Result<()> {
        let conn = self.lock();
        let Some(correct) = correct else {
            conn.execute(
                "UPDATE polls SET correct_option = NULL WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
            )?;
            return Ok(());
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT options_json FROM polls WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
                |r| r.get(0),
            )
            .optional()?;
        if json.is_some_and(|j| decode_json_strings(&j).iter().any(|o| o == correct)) {
            conn.execute(
                "UPDATE polls SET correct_option = ?3 WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id, correct],
            )?;
        }
        Ok(())
    }

    /// Every vote of a poll as `(option name, voter jid, vote time)`: options
    /// in the poll's order, newest vote first within an option. A voter who
    /// picked several options appears once per option; names that are not an
    /// option of the poll are left out.
    pub fn poll_voters(&self, chat_jid: &str, msg_id: &str) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let json: Option<String> = conn
            .query_row(
                "SELECT options_json FROM polls WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(Vec::new());
        };
        let options = decode_json_strings(&json);
        let mut stmt = conn.prepare(
            "SELECT voter_jid, options_json, ts FROM poll_votes \
             WHERE chat_jid = ?1 AND msg_id = ?2 ORDER BY ts DESC, voter_jid",
        )?;
        let rows = stmt.query_map(params![chat_jid, msg_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out: Vec<(usize, String, String, i64)> = Vec::new();
        for row in rows {
            let (voter, json, ts) = row?;
            for name in decode_json_strings(&json) {
                if let Some(index) = options.iter().position(|o| *o == name) {
                    out.push((index, name, voter.clone(), ts));
                }
            }
        }
        out.sort_by_key(|(index, ..)| *index);
        Ok(out.into_iter().map(|(_, name, voter, ts)| (name, voter, ts)).collect())
    }

    /// Records the selection of one voter (option names, repeats collapsed).
    /// The newest vote of a voter wins; an empty selection withdraws the vote.
    /// Returns `false`, storing nothing, when the poll limits the number of
    /// selectable options and the vote selects more than that.
    pub fn upsert_poll_vote(
        &self,
        chat_jid: &str,
        msg_id: &str,
        voter_jid: &str,
        selected: &[String],
        ts: i64,
        from_me: bool,
    ) -> Result<bool> {
        let selected = dedupe(selected);
        let conn = self.lock();
        if selected.is_empty() {
            conn.execute(
                "DELETE FROM poll_votes \
                 WHERE chat_jid = ?1 AND msg_id = ?2 AND voter_jid = ?3 AND ts <= ?4",
                params![chat_jid, msg_id, voter_jid, ts],
            )?;
            return Ok(true);
        }
        let limit: Option<u32> = conn
            .query_row(
                "SELECT selectable_count FROM polls WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
                |r| r.get(0),
            )
            .optional()?;
        if limit.is_some_and(|n| n > 0 && selected.len() > n as usize) {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO poll_votes (chat_jid, msg_id, voter_jid, options_json, ts, from_me) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(chat_jid, msg_id, voter_jid) DO UPDATE SET \
             options_json = excluded.options_json, ts = excluded.ts, from_me = excluded.from_me \
             WHERE excluded.ts >= poll_votes.ts",
            params![
                chat_jid,
                msg_id,
                voter_jid,
                encode_json_strings(&selected),
                ts,
                from_me
            ],
        )?;
        Ok(true)
    }

    /// The stored vote of one voter as `(selected option names, ts)`.
    pub fn poll_vote_of(
        &self,
        chat_jid: &str,
        msg_id: &str,
        voter_jid: &str,
    ) -> Result<Option<(Vec<String>, i64)>> {
        let row: Option<(String, i64)> = self
            .lock()
            .query_row(
                "SELECT options_json, ts FROM poll_votes \
                 WHERE chat_jid = ?1 AND msg_id = ?2 AND voter_jid = ?3",
                params![chat_jid, msg_id, voter_jid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(json, ts)| (decode_json_strings(&json), ts)))
    }

    /// Undoes a vote written at `written_ts` by putting `previous` back (or
    /// removing the vote when there was none). Does nothing when the stored
    /// vote is no longer the one written at `written_ts`, so a newer vote from
    /// another device is never overwritten. Returns whether anything changed.
    pub fn restore_poll_vote(
        &self,
        chat_jid: &str,
        msg_id: &str,
        voter_jid: &str,
        written_ts: i64,
        previous: Option<&(Vec<String>, i64)>,
        from_me: bool,
    ) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM poll_votes \
             WHERE chat_jid = ?1 AND msg_id = ?2 AND voter_jid = ?3 AND ts = ?4",
            params![chat_jid, msg_id, voter_jid, written_ts],
        )?;
        if removed == 0 {
            // A withdrawal leaves no row; any row now present is a newer vote.
            let present: i64 = conn.query_row(
                "SELECT COUNT(*) FROM poll_votes WHERE chat_jid = ?1 AND msg_id = ?2 AND voter_jid = ?3",
                params![chat_jid, msg_id, voter_jid],
                |r| r.get(0),
            )?;
            if present > 0 || previous.is_none() {
                return Ok(false);
            }
        }
        if let Some((selected, ts)) = previous {
            conn.execute(
                "INSERT OR IGNORE INTO poll_votes (chat_jid, msg_id, voter_jid, options_json, ts, from_me) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![chat_jid, msg_id, voter_jid, encode_json_strings(selected), ts, from_me],
            )?;
        }
        Ok(true)
    }

    /// Option names of a stored poll, for resolving vote hashes.
    pub fn poll_options(&self, chat_jid: &str, msg_id: &str) -> Result<Option<Vec<String>>> {
        let json: Option<String> = self
            .lock()
            .query_row(
                "SELECT options_json FROM polls WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| decode_json_strings(&j)))
    }

    /// The poll with its tally; `None` if the message is not a stored poll.
    pub fn poll_for(&self, chat_jid: &str, msg_id: &str) -> Result<Option<PollView>> {
        let conn = self.lock();
        let head: Option<(String, String, u32, Option<String>)> = conn
            .query_row(
                "SELECT question, options_json, selectable_count, correct_option FROM polls \
                 WHERE chat_jid = ?1 AND msg_id = ?2",
                params![chat_jid, msg_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((question, options_json, selectable_count, correct_name)) = head else {
            return Ok(None);
        };
        let mut options: Vec<(String, u32, bool)> = decode_json_strings(&options_json)
            .into_iter()
            .map(|name| (name, 0, false))
            .collect();

        let mut stmt = conn.prepare(
            "SELECT options_json, from_me FROM poll_votes WHERE chat_jid = ?1 AND msg_id = ?2",
        )?;
        let votes = stmt.query_map(params![chat_jid, msg_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        })?;
        for vote in votes {
            let (json, mine) = vote?;
            for name in decode_json_strings(&json) {
                if let Some(opt) = options.iter_mut().find(|o| o.0 == name) {
                    opt.1 += 1;
                    opt.2 |= mine;
                }
            }
        }
        let correct = correct_name.and_then(|c| options.iter().position(|o| o.0 == c));
        Ok(Some(PollView {
            question,
            options,
            selectable_count,
            correct,
        }))
    }

    /// Status updates with `ts >= since_ts`, grouped by poster. Newest group
    /// first, items oldest first.
    pub fn statuses_since(&self, since_ts: i64) -> Result<Vec<StatusGroup>> {
        let sql = format!(
            "SELECT {MESSAGE_COLS}, m.viewed FROM messages m \
             LEFT JOIN contacts ct ON ct.jid = m.sender_jid \
             WHERE m.chat_jid = ?1 AND m.ts >= ?2 ORDER BY m.ts ASC, m.rowid ASC"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![STATUS_JID, since_ts], |r| {
                Ok((message_from_row(r)?, r.get::<_, i64>(28)? != 0))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut groups: Vec<StatusGroup> = Vec::new();
        for (row, viewed) in rows {
            let viewed = viewed || row.from_me;
            match groups.iter_mut().find(|g| g.sender_jid == row.sender_jid) {
                Some(g) => {
                    g.all_viewed &= viewed;
                    if g.sender_name.is_none() {
                        g.sender_name = row.sender_name.clone();
                    }
                    g.items.push(row);
                }
                None => groups.push(StatusGroup {
                    sender_jid: row.sender_jid.clone(),
                    sender_name: row.sender_name.clone(),
                    all_viewed: viewed,
                    items: vec![row],
                }),
            }
        }
        // Items are ascending, so the last one is the newest of its group.
        groups.sort_by_key(|g| std::cmp::Reverse(g.items.last().map_or(0, |m| m.ts)));
        Ok(groups)
    }

    /// JIDs of the contacts saved in the phone's address book. Only the
    /// contact sync from the phone (app-state contact actions) ever sets
    /// `full_name`; push names learned from messages leave it empty.
    pub fn address_book_contacts(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT jid FROM contacts WHERE TRIM(COALESCE(full_name, '')) <> '' ORDER BY jid",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Marks a status as viewed; `true` only the first time.
    pub fn mark_status_viewed(&self, id: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE messages SET viewed = 1 WHERE chat_jid = ?1 AND id = ?2 AND viewed = 0",
            params![STATUS_JID, id],
        )?;
        Ok(changed > 0)
    }

    /// Housekeeping for reactions: deletes those older than `orphan_before_ts`
    /// whose message is not stored (it never arrived, or was purged), then
    /// keeps at most `per_chat_cap` reactions per chat (oldest dropped).
    /// Returns how many rows were removed.
    pub fn purge_reactions(&self, orphan_before_ts: i64, per_chat_cap: u32) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut removed = tx.execute(
            "DELETE FROM reactions WHERE ts < ?1 AND NOT EXISTS \
             (SELECT 1 FROM messages m \
              WHERE m.chat_jid = reactions.chat_jid AND m.id = reactions.msg_id)",
            params![orphan_before_ts],
        )?;
        let crowded = {
            let mut stmt = tx.prepare(
                "SELECT chat_jid FROM reactions GROUP BY chat_jid HAVING COUNT(*) > ?1",
            )?;
            stmt.query_map(params![per_chat_cap], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?
        };
        for chat in crowded {
            removed += tx.execute(
                "DELETE FROM reactions WHERE rowid IN (SELECT rowid FROM reactions \
                 WHERE chat_jid = ?1 ORDER BY ts DESC, rowid DESC LIMIT -1 OFFSET ?2)",
                params![chat, per_chat_cap],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Deletes statuses (and their reactions and receipts) older than `before_ts`.
    pub fn purge_statuses(&self, before_ts: i64) -> Result<StatusPurge> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let media_paths = {
            let mut stmt = tx.prepare(
                "SELECT local_path FROM messages \
                 WHERE chat_jid = ?1 AND ts < ?2 AND local_path IS NOT NULL",
            )?;
            stmt.query_map(params![STATUS_JID, before_ts], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?
        };
        tx.execute(
            "DELETE FROM reactions WHERE chat_jid = ?1 AND msg_id IN \
             (SELECT id FROM messages WHERE chat_jid = ?1 AND ts < ?2)",
            params![STATUS_JID, before_ts],
        )?;
        tx.execute(
            "DELETE FROM receipts WHERE chat_jid = ?1 AND msg_id IN              (SELECT id FROM messages WHERE chat_jid = ?1 AND ts < ?2)",
            params![STATUS_JID, before_ts],
        )?;
        let removed = tx.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND ts < ?2",
            params![STATUS_JID, before_ts],
        )?;
        tx.commit()?;
        Ok(StatusPurge {
            removed,
            media_paths,
        })
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn json_round_trip_with_escapes() {
        let items = vec![
            "plain".to_string(),
            "quo\"te \\ back\nnew\ttab".to_string(),
            "😀 emoji ✓".to_string(),
            String::new(),
            "ctl\u{1}".to_string(),
        ];
        assert_eq!(decode_json_strings(&encode_json_strings(&items)), items);
        assert_eq!(decode_json_strings("[]"), Vec::<String>::new());
    }
}
