//! WhatsApp engine: owns the whatsapp-rust client on a background tokio
//! runtime, mirrors chats/messages into `wa_store`, and talks to the UI via
//! [`Command`] / [`UiEvent`].

pub mod api;
mod account;
mod appstate;
mod avatar;
mod calls;
mod coalesce;
mod commands;
mod convert;
mod ephemeral;
mod events;
mod group_admin;
mod groups;
mod info;
mod history;
mod identity;
mod jid;
mod media;
mod merge;
mod msgops;
mod pending;
mod polls;
mod presence;
mod preview;
mod proxy;
mod runtime;
mod settings;
mod share;
mod status;
mod status_post;
mod sticker;
mod tasks;
mod translate;

pub use api::*;
pub use proxy::test_proxy_blocking;
pub use translate::detect_language;

/// Like [`start_with_settings`] with [`EngineSettings::default`]. The UI should
/// prefer [`start_with_settings`] with its persisted settings: messages that
/// arrive right after connecting are handled under the defaults until the
/// first [`Command::UpdateSettings`].
pub fn start(config: EngineConfig) -> anyhow::Result<(EngineHandle, UiEventReceiver)> {
    start_with_settings(config, EngineSettings::default())
}

/// Open the stores, start the background runtime and connect, with `settings`
/// in force from the first event on (downloads, receipts, notifications and
/// presence all follow them).
///
/// Returns immediately; progress (QR, connection, messages) arrives on the
/// returned receiver.
pub fn start_with_settings(
    config: EngineConfig,
    settings: EngineSettings,
) -> anyhow::Result<(EngineHandle, UiEventReceiver)> {
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(config.media_dir())?;
    let store = std::sync::Arc::new(Store::open(config.messages_db())?);
    // Messages left "sending" by a previous run will never be sent.
    match store.fail_stale_pending() {
        Ok(chats) if !chats.is_empty() => {
            log::info!("marked unsent messages in {} chats as failed", chats.len());
        }
        Ok(_) => {}
        Err(e) => log::warn!("could not fail stale pending messages: {e}"),
    }
    match status::purge_expired(&store, &config.media_dir(), events::now_secs()) {
        Ok(0) => {}
        Ok(n) => log::info!("purged {n} expired statuses"),
        Err(e) => log::warn!("could not purge expired statuses: {e}"),
    }
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();

    runtime::spawn_thread(runtime::Params {
        config: config.clone(),
        store: store.clone(),
        cmd_rx,
        ui: ui_tx,
        settings,
    })?;
    Ok((EngineHandle { cmd_tx, store, config }, ui_rx))
}
