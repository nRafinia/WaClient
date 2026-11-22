//! How this client presents itself to WhatsApp: the Windows desktop app
//! ("win_hybrid" row of the library's device-props table).

use whatsapp_rust::ClientProfile;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::wacore::store::device::{DevicePropsOverride, default_history_sync_config};

/// Registration-time device properties. Only applied by the library at the
/// initial pairing, so changing them later has no effect on a paired session.
pub fn device_props() -> DevicePropsOverride {
    DevicePropsOverride::new()
        .with_os("Windows")
        .with_platform_type(wa::device_props::PlatformType::UWP)
        .with_require_full_sync(true)
        .with_history_sync_config(wa::device_props::HistorySyncConfig {
            full_sync_days_limit: Some(365),
            on_demand_ready: Some(true),
            complete_on_demand_ready: Some(true),
            ..default_history_sync_config()
        })
}

/// Noise-handshake profile; not persisted by the library, so it must be set on
/// every start.
///
/// The current WhatsApp for Windows ("win_hybrid") is WA Web hosted in
/// WebView2: its handshake is the WEB user agent with the web version, and
/// only `DeviceProps` (see [`device_props`]) marks it as Windows. The legacy
/// `Platform::WINDOWS` profile expects the old native app's version scheme and
/// is rejected as "client outdated" when paired with the web version.
pub fn client_profile() -> ClientProfile {
    ClientProfile::web()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_props_are_the_windows_full_sync_row() {
        let p = device_props();
        assert_eq!(p.os.as_deref(), Some("Windows"));
        assert_eq!(p.platform_type, Some(wa::device_props::PlatformType::UWP));
        assert_eq!(p.require_full_sync, Some(true));
        let hsc = p.history_sync_config.expect("history config");
        assert_eq!(hsc.full_sync_days_limit, Some(365));
        assert_eq!(hsc.on_demand_ready, Some(true));
    }

    #[test]
    fn handshake_uses_the_web_user_agent() {
        let p = client_profile();
        assert_eq!(p.user_agent_platform, wa::client_payload::user_agent::Platform::WEB);
        assert!(p.include_web_info);
    }
}
