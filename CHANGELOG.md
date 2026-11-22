# Changelog

## 0.7.0

### Fixed
- Chats with a disappearing-messages timer set by the other person, a group admin or another device
  now keep the timer (newest change wins; a live message carrying a newer timer is adopted, never
  switched off by one) and show a small timer badge on the avatar in the chat list.

### Added
- App lock hardening: a corrupt or invalid `applock.json` now starts locked (fail closed) with a reset hint;
  passcode check runs off the UI thread; file drops and pastes are ignored while locked. A folders.json
  read error is moved aside to `folders.json.bad`; tab selection is saved lazily.
- Telegram-style shortcuts: Up in an empty composer edits your last message, Ctrl+Tab /
  Ctrl+Shift+Tab switch to the next / previous chat, Alt+Down / Alt+Up jump to the next / previous
  chat with unread messages (both follow the visible list and folder), and double-clicking a
  message replies to it. New `shortcuts.rs`.
- WhatsApp text formatting. Bubbles (incoming and outgoing) render `*bold*`, `_italic_`,
  `~strike~`, inline `` `code` ``, ```` ```monospace``` ```` blocks (verbatim) and `> quote` lines
  (bar) with WhatsApp's marker rules; lists keep their markers; links and phone numbers keep
  working inside formatted text. Composer: Ctrl+B, Ctrl+I, Ctrl+Shift+X (strike), Ctrl+Shift+M
  (monospace), Ctrl+Shift+. (quote) wrap or unwrap the selection (or insert an empty pair), and a
  small B / I / S / M / quote bar shows over the composer while text is selected. Chat-list and
  reply previews show text without the markers. New `format.rs`.
- Chat folders: Telegram-style tabs above the chat list. Built-in All / Unread / Personal / Groups
  with unread-chat counters (grey when every unread chat is muted) and custom folders (name,
  chosen chats, "include all unread / groups / personal" rules, "Exclude muted"; edit, delete,
  move left/right, "Add to folder" in the chat menu). Ctrl+Alt+1..9 selects a folder. Local and
  per profile (`folders.json`); can be hidden from the header menu. Search ignores the folder.
- App lock: optional passcode (Settings > Account > Privacy). Opaque lock screen over the whole
  window, Ctrl+L / header padlock, auto-lock after 1 min / 5 min / 1 h / 5 h idle, lock when
  minimised to tray; masked notifications and call banners while locked; wrong-attempt delay that
  survives restarts. Passcode stored only as a salted PBKDF2-HMAC-SHA256 hash in `applock.json`.
- Built-in proxy: Settings > Network > Proxy (SOCKS5 or HTTP, optional login, "Test connection",
  paste `socks5://` / `http://` / Telegram proxy links). One setting for all profiles, stored in
  `proxy.json` with the password encrypted by Windows DPAPI; applied after "Restart now".
  "(via proxy)" appears in the connection status.
- Proxy fails closed: unreadable, invalid or undecryptable proxy settings block all connections at start-up
  until you open the proxy settings or choose "Connect without proxy" (this run only).

## 0.6.0

### Added
- Incoming calls: a banner shows who is calling (voice / video, group calls too) with "Answer on
  your phone", Decline and Open chat; the taskbar button flashes when the window isn't focused.
  Missed calls appear in the chat as "Missed voice call" / "Missed video call" (counted as
  unread); declined calls as "Declined voice call". Calls can't be answered in the app.

## 0.5.1

### Fixed
- Ctrl+F (search in chat) and Ctrl+V (paste) now work while a chat is open even when the
  message box doesn't have focus.
- Starred-messages list pages through any number of messages starred in the same second.
- Messages starred on the phone before they reached this client (history sync) now show as starred
  once they arrive; parked stars are kept in memory for 7 days.

## 0.5.0

### Added
- Vote in polls (single or multiple answers; click again to change or withdraw your vote) and
  create polls from the attach menu (question, 2–12 options, "Allow multiple answers").
- Group management: New group (members, name, photo), add / remove members, make or dismiss
  admins, rename, description, group photo, invite link (copy / reset), group settings (only
  admins send / edit info, approve new members) and Exit group. Members that can't be added
  because of their privacy settings can be sent the invite link (up to 20 at a time).
- Join a group by link: paste a chat.whatsapp.com link (☰ › Join group by link…) or click one in
  a message; a preview shows the group first.
- Group-change notices in chats ("Ali added Sara", "You're now an admin", "Reza left", …).
- In "only admins can send" groups, or after leaving a group, the message box is replaced by a
  notice.

### Fixed
- A message sent from the app could appear above a photo it answered when the PC clock was
  behind; new outgoing messages are now never placed before the chat's latest message.
- The group info panel could keep showing old data after a change made while it was loading.

## 0.4.0

### Added
- Post your own statuses: text (up to 700 characters, with background colour and font), photo
  (JPEG, PNG, WebP) or video (MP4, MOV, 3GP, up to 60 seconds). Audience: My contacts (contacts
  synced from the phone), My contacts except..., or Only share with...; blocked contacts are
  always excluded.
- Send contacts as vCards (up to 20 at once), with a phone-number lookup on WhatsApp.
- Send a location: "Send my current location" through Windows Location, or paste coordinates or a
  Google Maps, OpenStreetMap or Apple Maps link. Incoming locations show their coordinates.
- Disappearing messages: per-chat timer (Off, 24 hours, 7 days, 90 days), a default timer for new
  chats, and notice rows in the chat when the timer changes.
- View-once sending for photos and videos.
- Phone numbers in messages are clickable: Chat on WhatsApp, Copy number, Look up profile. Local
  numbers starting with 0 use your account's country code.

### Fixed
- Profile edit icons are now drawn icons like the rest of the app.
- Message info was empty in direct chats: receipts from LID addresses are now matched, and the
  panel always shows the message status.

### Privacy & security
- View-once media you receive is still never viewable, forwardable or savable in the app.
- Disappearing messages are deleted locally (with their media, unless starred) once they expire;
  the check runs every minute.
- Status posts go only to the chosen audience; blocked contacts are excluded. The app cannot read
  the phone's own status-privacy setting.
- Phone-number lookups on WhatsApp are rate limited.

## 0.3.0

### Added
- Edit your own sent text messages (under 15 minutes old) with an "Editing message" strip and an
  "Edited" label.
- Selection mode (Forward, Star, Copy, Delete for me, Cancel) and a forward picker: forward up to
  20 messages to up to 5 chats; media is reused or re-uploaded; messages are marked "Forwarded".
- Pinned-message bar: pin a message for everyone for 24 hours, 7 days or 30 days.
- Starred messages: star marker and a starred messages panel, per chat or across all chats, with
  jump to the message.
- Message info panel: per-recipient receipts in groups, compact view in direct chats.
- @mention popup in groups (Up / Down, Enter / Tab; mentions are sent with the message); messages
  that mention you get an accent border.
- Per-chat drafts (text and reply target) per profile, persisted across restarts, with a red
  "Draft:" preview in the chat list.
- Chat-list menu: Mark as unread, Block / Unblock (direct chats only, 🚫 marker), Clear chat (with
  "Keep starred messages").
- Blocking and unblocking contacts: blocked contacts list and "Block a contact..." in Settings,
  with a confirmation.
- Edit your own profile name (25 characters), About text (139 characters); set or remove the
  picture.
- Settings regrouped into categories: Profile, Account (incl. privacy and blocked contacts),
  Chats, Notifications, Storage, App, Help.
- Esc closes one layer per press (dialog, mention popup, attach, viewer, search, info, side panel,
  edit, selection, reply, chat).
- Profile reordering: Move up / Move down, drag and drop, and Ctrl+1..5 to switch profiles.
- Icons drawn as vector paths.
- Keyboard shortcuts section in the README.

### Fixed
- Voice recording no longer stops when the audio buffer underruns or overruns (xruns). If the
  microphone stalls, the recording ends early but is kept.
- Recovery from an app-state version mismatch, so pin / archive / mute sync resumes.
- Delete for everyone window documented as about 2 days (WhatsApp server limit).

### Privacy & security
- View-once messages are shown as "open it on your phone" and are never viewable, forwardable or
  savable in the app.
- Forwarding and editing only act on messages stored locally and on your own messages; edit
  rejections from the server are surfaced as errors, not retried.

## 0.2.2

### Fixed
- Chat-row menu (pin / archive / mute) and message bubble menu actions did not fire because the
  menu closed before the callback ran.
- Archived chats are reachable from a new header menu (shows an empty state when there are none).
- Your own profile picture now loads after adding a profile.
- Incomplete app-state sync now triggers an automatic resync; pin / archive / mute show
  "Chat settings aren't synced with your phone yet - try again in a minute" until it completes.
- Pin / archive / mute run on a separate worker with a 30 s timeout.
- Media download HTTP 403 now retries via a reupload request.
- Unmuting a chat clears the mute correctly.
- Statuses no longer count towards unread.
- Commands are logged at info level (variant and redacted chat only).

### Added
- WhatsApp Business details in the info panel: category, description, address, email, websites,
  opening hours.
- Shared contact cards (vCard) with Message and Copy number.
- Info panel "Media, links and docs" tabs.
- Search inside a chat (Ctrl+F).
- Link previews: incoming previews are shown; optional outgoing previews.
- Paste images and files with Ctrl+V and drag and drop, with a preview before sending.
- Incognito quick toggle (ghost icon) in the chat-list header: turns off chat read receipts and
  status-view receipts for the active profile.
- Per-chat "Turn off read receipts for this chat" and "Mark as read now" (sends receipts for
  recent incoming messages: last 7 days, at most 200).
- "online / last seen" in the chat header (only when WhatsApp reports it).
- The active profile's name at the top of the chat list and in the window title (click to rename).
- Settings > Storage: shows the data folder and each profile's folder. "Change location..." moves
  all data after a restart: copy, verify (including SQLite integrity check), switch, delete old;
  on failure the old location stays. `location.json`, logs and the icon stay in
  `%APPDATA%\WaClient`.
- Status viewer downloads photos automatically and shows them inline; videos open in the external
  player.
- Group info shows member photos.

### Privacy & security
- Incoming link previews make no network requests, and the card is shown only if its link
  appears in the message text.
- Outgoing link previews are off by default (Settings > Privacy > "Send link previews"), because
  fetching a page reveals your IP address to that site. 3 s cap; local-network addresses are
  blocked. Known gaps: redirects and DNS rebinding.
- Read receipts can be turned off globally, per chat and with the incognito toggle. Status-view
  receipts are a separate setting.
- "Mark as read now" in a direct chat shows blue ticks to the sender only if the account-level
  WhatsApp "Read receipts" privacy setting is on (otherwise a silent read-self receipt is sent).
- Online presence off: WhatsApp can still see a brief "available" right after connecting (library
  limitation; the engine immediately sends "unavailable"). "Last seen" shown for others depends on
  WhatsApp privacy settings.

### Engineering
- Dev profile debug info reduced (`target/debug` had grown to 82 GB).

## 0.2.1

- Message context menu: Reply, React, Copy, Delete; delete for me / for everyone (within ~60 h).
- Clickable links (`http`, `https`, `mailto` only).
- Contact and group info panel from the chat header.
- Chat-list menu: Archive/Unarchive, Pin (max 3), Mute (8 h / 1 week / always), Mark as read;
  Archived list. Changes are synced with the phone.
- Deletes from other devices (delete for me, clear chat, delete chat) are applied.
- Chat-list profile pictures are loaded proactively.

## 0.2.0

- Multiple profiles (up to 5, online simultaneously), with a profile rail.
- Reactions and a full emoji picker.
- Statuses viewer (read-only) and read-only polls.
- Settings page: privacy, notifications, auto-download, font size, tray and autostart options.
- App icon and per-user installer.
- Crash-safe migration of 0.1.0 data into a "Main" profile.
- Single instance.

## 0.1.0

- Initial release: QR login, chats and messages, media, voice notes, notifications, system tray.
