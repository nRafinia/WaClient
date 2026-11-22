//! Cached message translations (one row per message and target language).

use rusqlite::{params, params_from_iter};

use crate::{Result, Store};

/// Ids per `IN (...)` query; stays far below SQLite's variable limit.
const ID_CHUNK: usize = 500;

/// A stored translation of one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTranslation {
    pub msg_id: String,
    pub text: String,
    /// Detected source language (ISO 639-1).
    pub source_lang: String,
}

impl Store {
    /// Stores (or replaces) the translation of a message into `target_lang`.
    pub fn put_translation(
        &self,
        chat_jid: &str,
        msg_id: &str,
        target_lang: &str,
        text: &str,
        source_lang: &str,
        created_at: i64,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO translations \
             (chat_jid, msg_id, target_lang, text, source_lang, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![chat_jid, msg_id, target_lang, text, source_lang, created_at],
        )?;
        Ok(())
    }

    /// Cached translations into `target_lang` for the given messages of a chat;
    /// messages without one are simply absent from the result.
    pub fn translations_for(
        &self,
        chat_jid: &str,
        ids: &[String],
        target_lang: &str,
    ) -> Result<Vec<CachedTranslation>> {
        let conn = self.lock();
        let mut out = Vec::new();
        for chunk in ids.chunks(ID_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT msg_id, text, source_lang FROM translations \
                 WHERE chat_jid = ?1 AND target_lang = ?2 AND msg_id IN ({marks})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let args = [chat_jid, target_lang].into_iter().chain(chunk.iter().map(String::as_str));
            let rows = stmt
                .query_map(params_from_iter(args), |r| {
                    Ok(CachedTranslation { msg_id: r.get(0)?, text: r.get(1)?, source_lang: r.get(2)? })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// The cached translation of one message, if any.
    pub fn translation_of(
        &self,
        chat_jid: &str,
        msg_id: &str,
        target_lang: &str,
    ) -> Result<Option<CachedTranslation>> {
        Ok(self.translations_for(chat_jid, &[msg_id.to_string()], target_lang)?.into_iter().next())
    }
}
