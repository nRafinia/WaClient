# WaClient — roadmap (Telegram-inspired features), agreed order 2026-11-21

Ordering criteria, in this order: value to the owner (Iran: connectivity first), risk (local-only
features carry no ban risk; anything changing what we send must stay protocol-compatible),
effort, and whether items can run as parallel agents with disjoint files.

## Phase 0 — before any new feature
- Live test of 0.4–0.6 on the secondary account (list: HANDOFF.md section 6). Fix what breaks first.

## Phase 1 — v0.7 "quick wins" — DONE in 0.7.0 (plus 2.1 proxy, done early)
| # | Item | Size | Files (to keep agents disjoint) | Notes |
|---|---|---|---|---|
| 1.1 | **Proxy feasibility study** (SOCKS5 / HTTP CONNECT inside the app) | read-only | — | Can whatsapp-rust 0.7 route its websocket + media HTTP through a proxy? Decides 2.1. |
| 1.2 | **Chat folders** (All / Unread / Personal / Groups + custom folders) | M | chat_list.slint, new folders_ui.rs, per-profile prefs (no DB migration) | Tabs above the chat list; unread badge per folder. |
| 1.3 | **App passcode lock** (PIN, auto-lock after idle, lock on minimise option) | S–M | new lock.slint + lock_ui.rs, prefs (hashed PIN, argon2/pbkdf2) | Notifications show no text while locked. |
| 1.4 | **Formatting toolbar + shortcuts** (Ctrl+B/I/Shift+X/Shift+M → `*bold*` `_italic_` `~strike~` ```` ``` ```` ) | S | composer.rs, chat_view.slint (composer area) | Check incoming rendering of all WhatsApp formats incl. quotes/lists. |
| 1.5 | **Telegram shortcuts**: Up = edit last message, Ctrl+Tab / Ctrl+Shift+Tab next/prev chat, Alt+Up/Down next unread chat, double-click = reply | S | main.slint key handling, app.rs | Coordinate with 1.4 (both near key handling) — run after 1.4 or split carefully. |
| 1.6 | **Cosmetic fixes**: admin badge height, centred "Read more", New-group Back button offset, settings category emoji → Path icons | S | contact_info.slint, group_new.slint, settings.slint, icons.slint | |

## Phase 2 — v0.8 "find & manage" — DONE in 0.8.0 (2.0–2.6; 2.1 proxy shipped in 0.7.0)

| # | Item | Size | Notes |
|---|---|---|---|
| 2.0 | **Translate** (Google gtx endpoint, via the proxy): per message + before sending; Settings section, ask target language on first use; Manual (default) / Auto (every incoming message not in the target language); language detection with a library (whatlang), not by script | M | Agreed 2026-11-22; message text goes to Google — only on click in Manual mode. |
| 2.1 | **Built-in proxy** (if 1.1 says feasible): settings page, per-profile or global, test-connection button | M | Highest value for Iran; may need a library patch/fork if transport isn't pluggable. |
| 2.2 | **Global search with filters** (media / links / files / voice / sender / date range) | M | Store FTS or indexed LIKE; reuse gallery code. |
| 2.3 | **Jump to date** (calendar in chat search) | S | |
| 2.4 | **Storage manager**: size per chat, clear media cache per chat / older than N days | S–M | Never delete starred media; never touch DB rows. |
| 2.5 | **Big emoji** for emoji-only messages; **font size / compact mode** | S | |
| 2.6 | **Voice playback speed** 1× / 1.5× / 2× (if not present) | S | |

## Phase 3 — v0.9 "look & feel" — DONE in 0.9.0
| # | Item | Size | Notes |
|---|---|---|---|
| 3.1 | **Themes / accent colours**, scheduled night mode | M | Theme tokens already exist. |
| 3.2 | **Chat wallpapers** (global + per chat) | S–M | |
| 3.3 | **Media viewer upgrade**: zoom, rotate, swipe through all chat media | M | |
| 3.4 | **Per-chat notification sound** + notification exceptions | S–M | |
| 3.5 | **Saved Messages** shortcut (chat with own JID) | S | |
| 3.6 | **Send later** (local schedule; only while the app runs — say so in the UI) | M | Goes through the normal send path at fire time. |
| 3.7 | **Export chat** (HTML / TXT / JSON + media folder) | M | |

## Phase 4 — later / bigger
- Open chat in a separate window; video picture-in-picture.
- Stickers: show and send saved stickers (webp).
- Polls: quizzes, per-voter "View votes"; groups: approve membership requests.
- Installer (Setup.exe) — as soon as Inno Setup is installed (`winget install JRSoftware.InnoSetup`).

## Not doing (protocol limits or decided against)
- Edit/delete without time limits, silent messages, bots, usernames, huge groups, server-side
  scheduling — not possible on WhatsApp.
- Message translation (needs an external service → privacy) — not planned.
- Live-location sending, answering/placing calls — see HANDOFF.md section 5.
