# WaClient

A native Windows desktop client for WhatsApp, written in Rust. It links to your account as a
"Linked device" (scan a QR code from the phone), like WhatsApp Web/Desktop.

> **WARNING: unofficial software.** WaClient is not affiliated with, endorsed by, or connected to
> WhatsApp or Meta. It speaks the WhatsApp multi-device protocol through a third-party library
> (`whatsapp-rust`) and mimics the official WhatsApp for Windows client. Using unofficial clients
> is against the WhatsApp Terms of Service and **can get your number temporarily or permanently
> banned**. Test with a secondary number, never your primary one. Use at your own risk.

## Features

- Pair by QR code (with "Reload QR code" when it expires); the session persists across restarts.
- Chat list with search, unread counters, profile pictures, group support.
- Send and receive text, with replies (quoting).
- Send and receive media: images, video, documents, audio; download, open, show in folder.
- Emoji-only messages (1-3 emoji) are shown large, without a bubble.
- Voice playback speed 1x / 1.5x / 2x (pill on the bubble; pitch rises with speed).
- Voice notes: record (Ogg/Opus) and play back. Buffer underruns/overruns (xruns) are ignored. If
  the microphone stops delivering audio (switched or unplugged), the recording ends early but is
  kept.
- Typing indicators (sent and received) and read receipts / message status.
- Initial history sync with progress, and loading older messages.
- Multiple profiles: up to 5 accounts, all online at the same time, switched from a profile rail.
  Each profile is paired separately and has its own data and settings. Profiles can be renamed;
  "Log out & remove" unlinks the device and deletes that profile's data.
- Reactions (quick reactions and a full emoji picker with search and recents), shown as chips on
  messages.
- Statuses viewer: recent statuses (last 24h) of your contacts; you can also post your own.
- Polls: vote (single or multiple answers; click again to change or withdraw) and create your
  own from the attach menu (question, 2–12 options, "Allow multiple answers"); live tallies.
- Incoming calls: banner with caller, voice / video, Decline and Open chat (answer on your
  phone); missed and declined calls appear in the chat.
- Groups: create a group (members, name, photo), add / remove members, make or dismiss admins,
  rename, edit the description, change or remove the photo, invite link (copy / reset), "Only
  admins can send messages" / "Only admins can edit info" / "Approve new members", exit group.
  Join by pasting a chat.whatsapp.com link or clicking one in a message (with a preview first).
  Group changes appear as notices in the chat ("Ali added Sara"). In admins-only groups the
  message box is replaced by a notice when you are not an admin.
- Settings page (per profile), grouped into categories: Profile, Account (privacy: read
  receipts, typing indicator, online presence, link previews, blocked contacts), Chats (font size,
  Enter-to-send, auto-download of media), Notifications (message preview, sound), Storage (data
  location, clearing the media cache), App (close to tray, start with Windows) and Help.
- Incognito quick toggle (ghost icon) in the chat-list header: turns off chat read receipts and
  status-view receipts for the active profile. Per-chat "Turn off read receipts for this chat" and
  "Mark as read now".
- "online / last seen" in the chat header of direct chats.
- The active profile's name is shown at the top of the chat list and in the window title (click
  to rename it).
- Icons are drawn as vector paths, so they scale cleanly.
- Settings > Storage: shows the data folder and each profile's folder; "Change location..." moves
  all data to another folder (after a restart).
- Status viewer downloads photos automatically and shows them inline; videos open in the external
  player. Group info shows member photos.
- Message actions via a context menu: Reply, React, Copy (to the clipboard), Edit, Forward, Pin,
  Star, Info and Delete. A delete dialog offers "Delete for me" and, for your own messages within
  about 2 days, "Delete for everyone".
- Edit your own sent text messages (under 15 minutes old, a WhatsApp limit). The composer shows an
  "Editing message" strip; edited messages carry an "Edited" label.
- Selection mode with a bottom bar (Forward, Star, Copy, Delete for me, Cancel) and a forward
  picker: forward up to 20 messages to up to 5 chats at once. Media is reused without re-uploading
  when possible, otherwise re-uploaded; forwarded messages are marked "Forwarded".
- Pinned-message bar: pin a message in a chat for everyone (24 hours, 7 days or 30 days).
- Starred messages: starred messages show a star marker. A starred messages panel lists them per
  chat (chat header menu) or across all chats (header menu ☰), and jumps to the message.
- Message info panel for your own messages: delivery, read and played receipts per recipient in
  groups; a compact view in direct chats. The panel always shows the message status.
- @mentions in groups: typing @ opens a suggestion popup (Up / Down, Enter or Tab to pick). Messages
  that mention you get an accent border.
- Per-chat drafts (text and reply target), kept per profile, persisted across restarts, and shown
  as a red "Draft:" preview in the chat list.
- Chat-list menu additions: Mark as unread, Block / Unblock (direct chats only) and Clear chat
  (with a "Keep starred messages" option). Blocked chats show a 🚫 marker.
- Block and unblock contacts; Settings > Account lists blocked contacts and has "Block a
  contact..." (with a confirmation).
- Edit your own profile (Settings > Profile): name (25 characters), About text (139 characters),
  and set or remove the profile picture.
- Profile reordering on the rail: Move up / Move down, drag and drop, and Ctrl+1..5 to switch.
- View-once messages are shown as a placeholder asking you to open them on your phone.
- Recovery from an app-state version mismatch, so pin / archive / mute keep syncing.
- Clickable links in messages (only `http`, `https` and `mailto` are opened).
- WhatsApp text formatting: `*bold*`, `_italic_`, `~strike~`, `` `code` ``, ```` ```monospace``` ````,
  `> quote` lines (bar), bullet and numbered lists. Shown in incoming and outgoing bubbles by
  WhatsApp's rules (`2*3*4` stays plain; links and phone numbers still work inside); chat-list
  previews show the text without markers. The composer has shortcuts and a small B / I / S / M /
  quote bar that appears over the composer while text is selected.
- Contact and group info panel, opened from the chat header.
- Chat-list menu: Archive / Unarchive, Pin (at most 3; not for archived chats), Mute (8 hours,
  1 week or always) and Mark as read. Archived chats are shown in a separate Archived list;
  archiving a pinned chat unpins it.
- Archive, pin and mute changes are synced with the phone, and deletes made on other devices
  (delete for me, clear chat, delete chat) are applied here.
- Archived chats are also reachable from the header menu (☰).
- Chat folders: tabs above the chat list (All, Unread, Personal, Groups) with unread-chat
  counters, plus your own folders (right-click a tab or press + : chosen chats, optional "all
  unread / groups / personal" rules, "Exclude muted"). Edit, delete and reorder from the tab menu,
  add a chat from its right-click menu (Add to folder). Local only, saved per profile in
  `folders.json`; hide the tabs from the header menu (☰). Search always covers all chats.
- WhatsApp Business details in the info panel (category, description, address, email, websites,
  opening hours) and "Media, links and docs" tabs.
- Shared contact cards (vCard) with Message and Copy number.
- Search inside a chat (Ctrl+F), with a calendar button to jump to the first message of a day.
- Search across all chats with filters: click the search box in the chat list for chips (All, Photos &
  videos, Links, Documents, Voice), a "From" picker and a date range (any time, today, last 7 / 30 days
  or a custom range from a calendar). Filters work without typing and list the newest matches first.
- Link previews: incoming previews are shown; outgoing previews are optional (Settings > Account).
- Paste images or files with Ctrl+V, or drag and drop them, with a preview before sending.
- Windows toast notifications and a system-tray icon (with "close to tray" on, closing the window
  keeps the app running in the tray).
- App lock: an optional passcode (4 to 32 characters) in Settings > Account > Privacy > App lock.
  WaClient starts locked and shows an opaque lock screen over the whole window; lock it by hand
  with Ctrl+L or the padlock in the chat-list header, or automatically after 1 min / 5 min /
  1 hour / 5 hours without keyboard or mouse use, or when it is minimised to the tray. While locked,
  toasts show only "WaClient - New message", the call banner shows "Incoming call" without the
  name, and opening a chat from a toast or banner waits for the unlock. After 5 wrong attempts
  the unlock is delayed (5 s, 10 s, 30 s, 1 min, 2 min, then 5 min), also across restarts.
  If `applock.json` is unreadable or damaged, WaClient fails closed: it starts locked with a message
  and no way to unlock; delete that file to reset the lock. File drops and pastes do nothing while
  locked.
  It is a screen lock only: the data is not encrypted, and messages keep arriving.
- Built-in proxy: Settings > Network > Proxy routes all WhatsApp traffic of every profile (chat
  socket, media, link previews, avatars) through a SOCKS5 or HTTP proxy, useful where WhatsApp is
  blocked. Enter host, port and an optional user name and password, or paste a `socks5://`,
  `socks5h://`, `http://` or Telegram proxy link (`tg://socks?...`, `https://t.me/socks?...`).
  "Test connection" reports the connect time or the error. The setting is app-wide and takes
  effect after a restart ("Restart now"); the connection status then shows "(via proxy)". The
  password is stored encrypted with Windows DPAPI for the current Windows user only; under another
  Windows account it reads as empty and has to be entered again.
  Fails closed: if proxy.json exists but cannot be read, is invalid, or its password cannot be
  decrypted (for example under another Windows account), WaClient does not connect at all and shows
  "Proxy settings couldn't be read. WaClient didn't connect, so your IP isn't exposed." with
  "Open proxy settings" (fix, save, then "Restart now") and "Connect without proxy" (this run
  only; the file is not changed).
- Logout (unlinks the device and wipes the local session).
- Post statuses: text (up to 700 characters, background colour and font), photo (JPEG, PNG, WebP)
  or video (MP4, MOV, 3GP, up to 60 seconds). Audience: My contacts (contacts synced from the
  phone), My contacts except..., or Only share with...; blocked contacts are excluded.
- Send contacts as vCards (up to 20 at once) after a phone-number lookup on WhatsApp
  (rate limited).
- Send a location: "Send my current location" (Windows Location; needs Windows Settings >
  Privacy > Location enabled for desktop apps), or paste coordinates or a Google Maps,
  OpenStreetMap or Apple Maps link. Received locations show their coordinates.
- Disappearing messages: per-chat timer (Off, 24 hours, 7 days, 90 days), a default timer for new
  chats and notice rows in the chat. Expired messages are deleted locally every minute, with their
  media unless starred.
- Send photos and videos as view once.
- Phone numbers in messages are clickable: Chat on WhatsApp, Copy number, Look up profile. Local
  numbers starting with 0 use your account's country code.
- Message translation: "Translate" in the message menu shows the translation under the bubble
  ("Show original" switches back). Settings > Chats > Translation sets the target language and the mode:
  Manual (default; asked for the language on first use) or Automatic, which translates every incoming
  message that isn't in your language. Message text is sent to Google Translate; results are cached locally.

## Known limitations

- Calls can't be answered or placed from the app (incoming calls are shown and can be declined).
- Quizzes and per-voter "View votes" lists are not supported; membership requests cannot be
  approved from the app.
- Statuses are still to be verified in real use; treat the viewer and posting as experimental.
- Only recent statuses (last 24h) are shown; older ones are purged.
- Every profile appears on the phone as the same "Windows" linked device type.
- Delete-for-me sync to the phone may be skipped in some new-style (LID) chats when the right
  identity is not known yet; direct chats use the phone-number key.
- Editing is limited to about 15 minutes after sending, and "Delete for everyone" to about 2 days.
  These are WhatsApp server limits, not app settings.
- View-once messages can only be opened on the phone. They are never viewable, forwardable or
  savable in WaClient.
- Videos and music are never downloaded automatically. Status photos are fetched when a status is
  viewed; status videos are never downloaded automatically.
- WhatsApp has no per-message timer; use view once for single messages instead.
- The phone's own status-privacy setting cannot be read by the app, so choose the audience when
  posting.
- Sending live location is not supported (only a one-time location). Short goo.gl map links are
  not supported.
- Video statuses are limited to 60 seconds.
- "Online / last seen" is shown only when WhatsApp reports it; it depends on the privacy settings
  of the contact and of your own account.

## Keyboard shortcuts

| Shortcut          | Action                                                            |
|-------------------|-------------------------------------------------------------------|
| Ctrl+F            | Search inside the open chat (also after clicking the chat)    |
| Ctrl+V            | Paste an image, file or text (also after clicking the chat)    |
| Esc               | Close one layer per press, topmost first (see below)              |
| Ctrl+1 .. Ctrl+5  | Switch to profile 1..5 on the rail                                |
| Ctrl+Alt+1 .. 9   | Select chat folder 1..9 (All is 1)                                |
| Ctrl+L            | Lock the app (only when an app-lock passcode is set)              |
| Enter             | Send the message (with "Enter to send" on)                        |
| Shift+Enter       | New line (with "Enter to send" on)                                |
| Ctrl+Enter        | Send the message (with "Enter to send" off; Enter is a new line)  |
| Ctrl+B / Ctrl+I   | Composer: bold / italic (selection or empty pair; again to remove) |
| Ctrl+Shift+X      | Composer: strikethrough                                           |
| Ctrl+Shift+M      | Composer: monospace (```)                                         |
| Ctrl+Shift+.      | Composer: quote (prefix the selected lines with "> ")             |
| Up                | Empty composer: edit your last message (within 15 minutes)        |
| Ctrl+Tab / Ctrl+Shift+Tab | Next / previous chat in the list (wraps around)           |
| Alt+Down / Alt+Up | Next / previous chat with unread messages                         |
| Double-click      | On a message: reply to it                                         |
| Up / Down         | Move in the @mention popup                                        |
| Enter / Tab       | Pick the highlighted @mention                                     |

Esc closes, in order: a forward or pin dialog, the mention popup, the attach preview (not while
sending), the status viewer, chat search, contact info, the message info or starred panel, edit
mode, selection mode, the reply, and finally the open chat. Menus and the emoji picker close
themselves first. The status viewer also uses Left / Right arrows.

## Requirements

- Windows 10/11 x64.
- Stable Rust 1.94 or newer (via `rustup`). Nightly is not needed (the `simd` feature is disabled).
- Visual Studio Build Tools with the "Desktop development with C++" workload (MSVC).
- CMake, needed to build libopus from source. The one bundled with Visual Studio works; see
  `.cargo/config.toml`.

## Build and run

```powershell
cargo build --release -p waclient
.\target\release\waclient.exe
```

On first start a QR code is shown. On the phone: WhatsApp > Settings > Linked devices >
Link a device, then scan it.

For a portable folder and zip (`dist\WaClient-<version>-win-x64.zip`):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1
# add -DryRun to only print the steps
```

## Install

- **Zip (portable):** extract `WaClient-<version>-win-x64.zip` anywhere and run `WaClient.exe`.
- **Installer:** run `WaClient-<version>-setup.exe`. It installs per user (no admin rights) to
  `%LOCALAPPDATA%\Programs\WaClient`, adds a Start-menu shortcut (needed for correct toast
  notification attribution) and offers optional desktop shortcut and "Start WaClient when I sign in"
  tasks. Build it with `scripts\package.ps1 -Installer`; this needs Inno Setup 6
  (`winget install JRSoftware.InnoSetup`).
- **Autostart:** the installer task writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
  value `WaClient` = `"<install dir>\WaClient.exe" --minimized`. With `--minimized` the app is
  expected to start hidden in the system tray. The "Start with Windows" switch in Settings writes
  and removes the same value.
- **Uninstall:** Settings > Apps > WaClient. This removes the app, shortcuts and the autostart value,
  and asks whether to also delete your data in `%APPDATA%\WaClient` (default: keep).

To regenerate the icons from `assets/icon.svg` geometry:
`cargo run --manifest-path scripts/gen-icon/Cargo.toml`.

### Debug demo mode

Debug builds only: `WACLIENT_DEMO=1` starts the UI with fake chats and no network, which is
handy for UI work.

```powershell
$env:WACLIENT_DEMO = "1"; cargo run -p waclient
```

## Tests

```powershell
cargo test --workspace
```

## Data, reset and logs

By default everything lives in `%APPDATA%\WaClient`. The location can be changed (see "Storage
location" below). Paths in this table are relative to the current data folder:

| Path                        | Contents                                              |
|-----------------------------|-------------------------------------------------------|
| `profiles.json`             | profile registry (names, colours, order, last active) |
| `emoji_recents.json`        | recently used emoji (shared by all profiles)          |
| `applock.json`              | app-lock salted hash and settings (absent: no lock)   |
| `proxy.json`                | proxy settings, password DPAPI-encrypted (absent: no proxy) |
| `profiles/<id>/session.db`  | protocol/session state (keys, device)                 |
| `profiles/<id>/messages.db` | chats, messages, reactions, polls, statuses           |
| `profiles/<id>/media/`      | downloaded media and cached avatars                   |
| `profiles/<id>/settings.json` | that profile's settings                             |
| `profiles/<id>/drafts.json` | unsent drafts per chat (text and reply target)        |

Bootstrap files always stay in `%APPDATA%\WaClient` and are never moved:

| Path                        | Contents                                              |
|-----------------------------|-------------------------------------------------------|
| `location.json`             | where the data folder is (absent: data is in `%APPDATA%\WaClient`) |
| `waclient.log`              | log file (release builds)                             |
| `waclient-icon.png`         | app icon written for toasts and the tray              |

**Forgot the app-lock passcode:** there is no recovery. Quit WaClient (also from the tray) and
delete `applock.json` in the data folder (by default `%APPDATA%\WaClient\applock.json`; the
whole file is the lock entry: hash, salt, auto-lock and the failed-attempt counter). The lock is
then off and everything else is untouched, because the passcode never encrypted anything.

`<id>` is an 8-character hex id from `profiles.json`. A corrupt `profiles.json` or `settings.json`
is renamed to `profiles.json.corrupt-<timestamp>` / `settings.json.bad` and defaults are used.

**Upgrading from 0.1.0:** on first start, if there is no `profiles.json` but the old
`session.db` / `messages.db` / `media/` sit directly in `%APPDATA%\WaClient`, they are moved
into a new profile named "Main" (nothing is copied or lost). If the move fails it is rolled back
and "Main" keeps using the root folder.

### Storage location

Settings > Storage shows the data folder and the folder of each profile. "Change location..."
asks for a folder, creates a `WaClient` folder inside it and moves **all** data there (every
profile). The move is safe and happens on the next start, before any database is opened:

1. The app records the pending move in `location.json` and restarts.
2. The data is copied to the new place.
3. The copy is verified, including an SQLite integrity check of the databases.
4. Only then is the new location recorded and the old data deleted.

If a step fails, the old location stays in use and nothing is deleted. Close other programs that
may lock the files (backup tools, antivirus) and try again.

**Moving back:** use "Change location..." again and pick the folder that contains `WaClient`
under `%APPDATA%` (that is, `%APPDATA%` itself). Manual fallback: quit the app, copy the data
back into `%APPDATA%\WaClient`, then delete `location.json`; without it the app uses
`%APPDATA%\WaClient` as the data folder. Without copying the data back first, the app starts
with no profiles.

**Reset one profile:** use "Log out & remove" on it in the app (unlinks the device and deletes
`profiles/<id>`). To only free disk space, use Settings > Storage: it shows what uses the space and
clears downloaded media per chat, by type, by age, or all at once ("Clear all media cache").
Starred messages' media, files still being sent, avatars and the messages themselves are kept;
cleared media can be downloaded again while it is still on WhatsApp's servers.

**Reset everything:** quit the app (also from the tray) and delete `%APPDATA%\WaClient`. You will
have to link every device again. Prefer the in-app logout when possible so the phone drops the
link too; otherwise remove the stale entry under Linked devices on the phone.

Logging: debug builds honour `RUST_LOG` (for example `$env:RUST_LOG = "debug"`). Release builds
have no console and write `waclient.log` (rotated at 5 MB); the default filter is
`warn,waclient=info,wa_engine=info,wa_store=info`.

## Privacy

Each profile has its own settings (defaults: all on).

- **Read receipts (global):** off means chat read receipts are not sent; opening a chat only
  resets the local unread counter. When on, chat receipts are sent only for the active profile's
  open chat while the window is focused; chats of other profiles are never marked read.
- **Per chat:** "Turn off read receipts for this chat" suppresses automatic receipts for that
  chat only (a local setting, nothing is sent).
- **Incognito (ghost icon in the chat-list header):** one click turns off chat read receipts and
  status-view receipts for the active profile; click again to turn them back on.
- **Mark as read now:** an explicit action that sends read receipts for the recent incoming
  messages of a chat (last 7 days, at most 200) even when receipts are off. Caveat: in a direct
  chat the sender sees blue ticks only if the account-level WhatsApp privacy setting "Read
  receipts" is on. If it is off, a silent "read-self" receipt is sent and the sender sees nothing.
- **Status view receipts:** a separate setting, independent of chat read receipts and not
  affected by the account-level WhatsApp "Read receipts" setting. A status is marked as viewed
  (once per status per session) only when it is actually displayed in the viewer of the active
  profile; with the setting off it is only marked as viewed locally.
- **Typing indicator:** off means "typing..." is not sent.
- **Online presence:** off means the account does not announce itself as online. Limitation: the
  library sends its own "available" presence on every connect before the connection is ready;
  the engine answers each with an immediate "unavailable", but WhatsApp can see a brief
  "available" right after connecting, and this cannot be avoided.
- **Online / last seen of others:** requested for the direct chat you have open and for the
  info panel, at most once per 5 minutes per contact. WhatsApp reports it only when the
  contact's privacy settings allow it (and your own settings can hide it too); otherwise
  nothing is shown.
- Auto-download: images and voice notes on, documents off, limit 16 MB (1-100 MB).
- **Link previews:** incoming previews make no network requests, and a card is shown only if its
  link appears in the message text. Sending previews ("Send link previews") is **off by default**
  because fetching the page reveals your IP address to that site. Fetches are capped at 3 s and
  local-network addresses are blocked; known gaps are redirects and DNS rebinding.

## Architecture

Cargo workspace with three crates:

- `crates/store` (`wa_store`): SQLite store (rusqlite) for chats and messages.
- `crates/engine` (`wa_engine`): wraps `whatsapp-rust` on a background tokio runtime. Handles
  pairing, sync, send/receive, media, avatars, groups. The identity mimics WhatsApp for Windows
  (UWP device props, 365 days of history sync, WEB handshake profile).
- `app` (`waclient`): Slint UI, tray, notifications, audio.

Data flow: the UI never touches the protocol library. It sends `Command`s (SendText, SendMedia,
MarkChatRead, DownloadMedia, EditMessage, ForwardMessages, Logout, ...) through an `EngineHandle`. The engine writes incoming
and outgoing messages into the shared store and emits `UiEvent`s (Qr, Paired, ChatsChanged,
MessagesChanged, Notify, MediaReady, Fatal, ...) on a channel. The UI forwards those to the Slint
event loop and re-reads chats and messages from the store.

`rusqlite` is pinned to 0.39 so it shares `libsqlite3-sys` 0.37 with whatsapp-rust's diesel; two
copies would fail to link.

## Troubleshooting

- **CMake not found / libopus fails to build:** `.cargo/config.toml` sets `CMAKE` to the copy in
  Visual Studio 2026 Enterprise. Edit that path to your `cmake.exe` (another VS edition, or a
  standalone CMake).
- **"Client outdated":** WhatsApp rejected the emulated client version. Update the
  `whatsapp-rust` dependency (or the version in the engine identity) and rebuild.
- **QR expired:** after a few unscanned codes the engine stops regenerating; click
  "Reload QR code".
- **Linking limits:** an account can have only a limited number of linked devices, and WhatsApp
  may refuse new links. Remove old linked devices on the phone.
- **"Chat settings aren't synced with your phone yet - try again in a minute":** pin, archive and
  mute need the app-state sync from the phone, which was not complete yet. The app resyncs
  automatically; wait a minute and retry (keep the phone online).
- **Disk full / huge `target` folder:** debug builds can grow large. Delete `target/debug/incremental`
  or run `cargo clean`.
- **"Change location..." did not take effect and the data is still in the old folder:** the move
  runs on the next start. If copying or verification fails (target not writable, disk full, files
  locked, integrity check failed), the app keeps using the old location and deletes nothing.
  Check `waclient.log` in `%APPDATA%\WaClient` for the reason, fix it (free space, pick a local
  writable folder, close programs that lock the files) and try again.
- **Data appears missing after editing `location.json`:** it points to a folder without your
  `profiles`. Restore the previous `location.json` or point it to the folder that holds them.
- **No tray icon or notifications:** the app logs a warning and continues without them; check
  `waclient.log`.

## License

See `LICENSE-NOTICE.txt` in the package for third-party components. Slint is triple-licensed
(GPL-3.0-only, Slint Royalty-free 2.0, or commercial). The royalty-free option is free for
desktop apps but requires visible attribution to Slint; GPLv3 would require this app to be GPLv3.
Pick one before distributing binaries: <https://slint.dev/terms-and-conditions>.
