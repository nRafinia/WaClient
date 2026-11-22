//! Decisions derived from [`EngineSettings`]; pure so they are unit-testable.

use wa_store::MessageKind;

use crate::api::EngineSettings;

const MIB: u64 = 1024 * 1024;
/// Whatever the user configures, nothing larger is fetched unasked.
pub const AUTO_DOWNLOAD_CEILING_MB: u32 = 100;

/// Largest automatic download in bytes; `0` disables automatic downloads.
pub fn auto_download_cap_bytes(s: &EngineSettings) -> u64 {
    u64::from(s.auto_download_max_mb.min(AUTO_DOWNLOAD_CEILING_MB)) * MIB
}

/// Whether an incoming message's media is fetched on arrival. Videos and
/// music are never fetched unasked; a missing size counts as too large.
pub fn should_auto_download(kind: MessageKind, size: Option<i64>, s: &EngineSettings) -> bool {
    let enabled = match kind {
        MessageKind::Image | MessageKind::Sticker => s.auto_download_images,
        MessageKind::Voice => s.auto_download_voice,
        MessageKind::Document => s.auto_download_documents,
        MessageKind::Text | MessageKind::Video | MessageKind::Audio | MessageKind::Other => false,
    };
    let cap = auto_download_cap_bytes(s);
    enabled
        && cap > 0
        && size
            .and_then(|n| u64::try_from(n).ok())
            .is_some_and(|n| n <= cap)
}

/// Whether read receipts may be sent automatically for a chat.
pub fn effective_read_receipts(s: &EngineSettings, chat_receipts_off: bool) -> bool {
    s.send_read_receipts && !chat_receipts_off
}

/// Whether viewing a status is reported to its poster (independent of the
/// chat read receipts).
pub fn may_send_status_receipts(s: &EngineSettings) -> bool {
    s.send_status_receipts
}

/// Whether typing indicators may be sent.
pub fn may_send_typing(s: &EngineSettings) -> bool {
    s.send_typing
}

/// Whether `UiEvent::Notify` may be emitted.
pub fn may_notify(s: &EngineSettings) -> bool {
    s.notifications
}

/// Whether we may announce ourselves as online.
pub fn may_send_presence(s: &EngineSettings) -> bool {
    s.send_presence
}

/// How long after a library-triggered `available` we assert `unavailable`
/// once more: the library's own presence send can land after our handler.
pub const PRESENCE_REASSERT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `unavailable` must be (re-)asserted after an event after which the
/// library sends `available` by itself (reconnect, push-name sync/change).
pub fn must_assert_unavailable(s: &EngineSettings) -> bool {
    !may_send_presence(s)
}

/// How presence must change when the settings go from `old` to `new` while
/// connected: `Some(true)` = become available, `Some(false)` = unavailable,
/// `None` = leave it.
pub fn presence_change(old: &EngineSettings, new: &EngineSettings) -> Option<bool> {
    (may_send_presence(old) != may_send_presence(new)).then(|| may_send_presence(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: i64 = 1024 * 1024;

    #[test]
    fn presence_off_asserts_unavailable_after_library_available() {
        let on = EngineSettings::default();
        let off = EngineSettings { send_presence: false, ..EngineSettings::default() };
        assert!(!must_assert_unavailable(&on));
        assert!(must_assert_unavailable(&off));
    }

    #[test]
    fn status_receipts_follow_only_their_own_setting() {
        let on = EngineSettings::default();
        assert!(may_send_status_receipts(&on));
        let chats_off = EngineSettings { send_read_receipts: false, ..EngineSettings::default() };
        assert!(may_send_status_receipts(&chats_off));
        let status_off = EngineSettings { send_status_receipts: false, ..EngineSettings::default() };
        assert!(!may_send_status_receipts(&status_off));
        assert!(effective_read_receipts(&status_off, false));
    }

    #[test]
    fn chat_receipts_need_global_on_and_chat_not_off() {
        let on = EngineSettings::default();
        let off = EngineSettings { send_read_receipts: false, ..EngineSettings::default() };
        assert!(effective_read_receipts(&on, false));
        assert!(!effective_read_receipts(&on, true));
        assert!(!effective_read_receipts(&off, false));
        assert!(!effective_read_receipts(&off, true));
    }

    #[test]
    fn defaults_fetch_small_images_and_voice_but_not_documents() {
        let s = EngineSettings::default();
        assert!(should_auto_download(MessageKind::Image, Some(MB), &s));
        assert!(should_auto_download(MessageKind::Sticker, Some(10), &s));
        assert!(should_auto_download(MessageKind::Voice, Some(MB), &s));
        assert!(!should_auto_download(MessageKind::Document, Some(MB), &s));
        assert!(!should_auto_download(MessageKind::Video, Some(1), &s));
        assert!(!should_auto_download(MessageKind::Audio, Some(1), &s));
        assert!(!should_auto_download(MessageKind::Text, Some(1), &s));
    }

    #[test]
    fn size_cap_is_inclusive_and_unknown_size_is_refused() {
        let s = EngineSettings::default();
        assert!(should_auto_download(MessageKind::Image, Some(16 * MB), &s));
        assert!(!should_auto_download(MessageKind::Image, Some(16 * MB + 1), &s));
        assert!(!should_auto_download(MessageKind::Image, None, &s));
        assert!(!should_auto_download(MessageKind::Image, Some(-5), &s));
    }

    #[test]
    fn documents_follow_their_switch_and_the_cap() {
        let s = EngineSettings {
            auto_download_documents: true,
            auto_download_max_mb: 5,
            ..EngineSettings::default()
        };
        assert!(should_auto_download(MessageKind::Document, Some(5 * MB), &s));
        assert!(!should_auto_download(MessageKind::Document, Some(6 * MB), &s));
    }

    #[test]
    fn per_kind_switches_are_independent() {
        let s = EngineSettings {
            auto_download_images: false,
            auto_download_voice: false,
            ..EngineSettings::default()
        };
        assert!(!should_auto_download(MessageKind::Image, Some(1), &s));
        assert!(!should_auto_download(MessageKind::Sticker, Some(1), &s));
        assert!(!should_auto_download(MessageKind::Voice, Some(1), &s));
    }

    #[test]
    fn cap_has_a_hard_ceiling_and_zero_disables() {
        let huge = EngineSettings {
            auto_download_max_mb: u32::MAX,
            auto_download_documents: true,
            ..EngineSettings::default()
        };
        assert_eq!(auto_download_cap_bytes(&huge), 100 * 1024 * 1024);
        assert!(!should_auto_download(MessageKind::Document, Some(101 * MB), &huge));
        assert!(should_auto_download(MessageKind::Document, Some(100 * MB), &huge));

        let off = EngineSettings {
            auto_download_max_mb: 0,
            ..EngineSettings::default()
        };
        assert!(!should_auto_download(MessageKind::Image, Some(0), &off));
    }

    #[test]
    fn presence_follows_its_switch_and_only_changes_on_toggle() {
        let on = EngineSettings::default();
        let off = EngineSettings {
            send_presence: false,
            ..EngineSettings::default()
        };
        assert!(on.send_presence, "online by default");
        assert!(may_send_presence(&on));
        assert!(!may_send_presence(&off));
        assert_eq!(presence_change(&on, &off), Some(false));
        assert_eq!(presence_change(&off, &on), Some(true));
        assert_eq!(presence_change(&on, &on), None);
        assert_eq!(presence_change(&off, &off), None);
    }

    #[test]
    fn receipts_typing_and_notifications_follow_the_switches() {
        let on = EngineSettings::default();
        assert!(effective_read_receipts(&on, false) && may_send_typing(&on) && may_notify(&on));
        let off = EngineSettings {
            send_read_receipts: false,
            send_typing: false,
            notifications: false,
            ..EngineSettings::default()
        };
        assert!(!effective_read_receipts(&off, false));
        assert!(!may_send_typing(&off));
        assert!(!may_notify(&off));
    }
}
