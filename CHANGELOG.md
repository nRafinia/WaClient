# Changelog

## 0.12.0

### Added

- The total unread count now shows as a red badge on the Windows taskbar button ("99+" above 99). It
  is cleared at zero and while the app lock is on.

### Fixed

- A dropped connection, with or without a proxy ("peer closed connection without sending TLS close_notify"),
  no longer leaves the app "Disconnected" until you press Reconnect. It now shows "Connecting…" and
  retries by itself, through the proxy, at 1, 2, 5, 10, 30, 60 s intervals; a lost proxy tunnel is
  treated as an ordinary disconnect, and the proxy connection sends TCP keepalive probes so idle
  tunnels survive and dead ones are noticed. Messages sent during the outage stay pending and go
  out by themselves once the connection is back (up to 24 hours old), instead of turning "failed".

- In right-to-left text (Persian, Arabic, Hebrew) the Left and Right arrow keys of the message
  composer, also in pop-out windows, now move the cursor the way they point on screen, with Shift to
  select and Ctrl to jump by word. A paragraph's first strong letter decides the direction, also
  for mixed text; emoji and accented letters are stepped over as a whole.

- The profile right-click menu (Rename, Log out & remove) no longer closes by itself: background
  refreshes (unread counts, connection state, avatars) now update the rail in place instead of
  rebuilding it.

## 0.11.0

### Added

- The proxy can be set on the login (QR) screen, so pairing works where WhatsApp is blocked:
  "Connection problems? Set up a proxy" opens the proxy form in a dialog (first run and when adding
  a profile), and a hint appears when no QR code arrives within 15 seconds and no proxy is set.
  Saving a changed proxy there relaunches WaClient at once with the proxy active, back on the same QR screen (the profile being
  added), not on another profile.
- Pop-out chat windows are closer to the main window: @mention suggestions in groups, the Stickers
  tab of the emoji picker, the chat's wallpaper (own or default, with dim and blur; follows changes),
  full-size inline pictures (the newest rows, memory-capped), the "typing..." indicator (same
  privacy setting) and drafts. A draft is shared per chat with the main window: text and reply
  typed in one show in the other when you switch back to the chat, and nothing is saved twice.
- After the app lock is released with the passcode, the pop-outs that were open when it locked
  come back (same chats, same size and place when known). They are never shown before a
  successful unlock, and a normal close or quit is not restored.

### Fixed

- Scrolling up to older messages no longer makes the chat flicker and stop loading: a page is now put in front of the list without rebuilding it, the message at the top stays where it was (pinned to its row, not to a pixel guess that the list's changing height estimates threw off), only one page loads at a time, and loading carries on until the real start of the chat; the pop-out windows load history the same way.
- Chats from business accounts (templates, buttons, lists, products, order and reply messages) and any
  message type WaClient cannot show yet no longer go missing: they are shown as text with their buttons,
  or as "This message type isn't supported in WaClient yet.". Messages already dropped are not recovered;
  the chat appears with its next message.
- Auto-lock counts activity in pop-out chat windows (and "lock on tray" waits until no window is
  shown); duplicate "asked to join" notices that arrive out of order fold into one row.
- The tray tooltip and badge no longer show the unread count while the passcode lock is on.
- The forward picker now checks groups you have not opened this session (up to 20, paced), so
  admins-only and left groups are greyed out up front.
- "Asked to join" notices fold duplicates by requester set within a 5-minute sliding window, with an
  id that is stable across Rust versions.
- Messages (and the group description) that mix Persian/Arabic and English lines now align every line by
  its own direction, like WhatsApp, instead of one alignment for the whole message.

## 0.10.0

### Added

- Open a chat in its own window: "Open in new window" in the chat-list row menu and in the chat
  header menu. The pop-out shows the messages live and has the composer (formatting shortcuts,
  emoji, undo, reply), reactions, media opening and mark-as-read; up to 4 windows, one per chat
  (opening again brings it to the front). It follows the theme and closes when the app locks.
  Search, info panel, polls voting, translation, stickers, attachments and voice notes stay in the
  main window.
- Chat wallpaper: a Blur slider (0-20) for custom images with a live preview in the dialog. The
  original image is kept, so blur can go back to off. The empty "Select a chat" pane now shows the
  default wallpaper too.
- Group admins: with "Approve new members" on, the group info shows "Pending requests (N)"; the
  list shows each person (name, phone, when they asked) with Approve / Reject, plus "Approve all" /
  "Reject all" (asks first). "Sara asked to join" appears as a notice in the chat; a cancelled
  request shows no notice.
- Polls: "View votes" under a poll bubble opens a list of each option with its voters (avatar,
  name, "You" for your own vote) and when they voted; it updates live while open.
- Quizzes: the Create poll dialog has a "Quiz mode" switch (single answer, mark the correct
  option; sent like WhatsApp Web's quizzes). Received and own quizzes show a green check on the
  correct option, a red cross on your wrong choice and "Correct answer: ..." once you have voted
  (always for your own quiz). The correct answer is stored with the poll (store migration 13).
- Stickers: the emoji picker has an Emoji | Stickers switch. The Stickers tab shows your
  favourites, then the stickers you recently received or sent (newest first, each file once); click
  one to send it to the open chat (as a reply while the reply bar is up). Right-click a sticker in
  the grid to add it to or remove it from the favourites; the message menu of a sticker has
  "Add to favourites" / "Remove from favourites". A sticker is sent with its original upload when
  it is less than 14 days old, otherwise its saved file is uploaded again. Favourites are stored
  per profile (store migration 14).

### Fixed

- Re-sending a sticker or forwarding media counts the age of the upload itself (not of the message)
  against the 14-day reuse limit, so a copy of an old upload is uploaded again instead of arriving
  as a dead download. A chat-list search refresh requested while "Load more" runs now happens
  after it finishes.
- Messages in right-to-left languages (Persian, Arabic, Hebrew) are right-aligned in the chat
  history like WhatsApp, including the quoted snippet, translation, caption, reply bar, pinned bar
  and chat-list preview. Each message follows its own first strong letter; a Persian message with
  an English word stays right-to-left.
- Text fields now write from the right for right-to-left text (Persian, Arabic, Hebrew, Urdu...):
  the message composer (also while editing a message), the chat-list search and the in-chat search
  right-align when the text starts with a right-to-left letter and go back to the left for other
  text. A multi-line message uses one alignment for the whole field. The same goes for polls and
  quizzes (question, options, votes list, create dialog), group names, descriptions and member
  names, the emoji search and global search snippets.
- Passcode lock: a menu that was open when the app locked no longer shows above the lock screen,
  and the window title is just "WaClient" while locked (no profile name or unread count).
- Forwarding: groups where only admins can send, and groups you left, are greyed out in the forward
  picker ("Only admins can send") and skipped when sending. A group's setting is known once it was
  opened in this session.
- Search: Esc in the chat-list search clears the text and filters and hides the filter row; shown
  results refresh about a second after new messages arrive; the filter chips (Media, Links, Docs,
  Voice) all fit without scrolling.
- Settings: the Account (key) and Notifications (bell) icons are larger and thicker.

## 0.9.0

### Added

- Send later: right-click the Send button (or its small chevron) > "Send later..." with quick picks
  (In 1 hour, Tonight 21:00, Tomorrow 09:00) and a local date/time (future, at most 1 year). Text
  only; mentions, formatting and reply are kept. WhatsApp cannot schedule, so WaClient sends the
  message itself, only while it is running. Items persist per profile in `scheduled.json`; a timer
  (15 s) sends due ones on their own profile; items that came due while the app was closed are held
  behind an "N scheduled messages are overdue" banner (Send now / Review) instead of being sent
  silently (the banner counts every profile, e.g. "3 scheduled messages are overdue (Work: 2)", and
  Review switches to the profile that has them); the composer gate is re-checked when an item fires
  (an unknown group is asked for its flags first, one tick later). Quick picks are recomputed when
  confirmed and every time must still be in the future. An item due while its profile is not
  connected stays ("Waiting for connection") and is sent once connected. A clock icon in the chat header and
  the chat-list menu > "Scheduled messages" list them (Edit time, Send now, Delete). New
  `schedule_ui.rs`, `schedule.slint`.
- Notification sounds: Settings > Notifications picks the sound (Windows default or one of five built-in
  tones; a tap previews it). Per chat, the info panel has a Notifications section: sound (Default /
  None / a tone, with a play button), "Show preview text" (Default / On / Off) and "Always notify"
  (also when the chat is muted). Tones are synthesized, not bundled; a tone plays on its own audio
  stream, so a voice note keeps playing, and is skipped only while another tone is still sounding.
  Muted chats stay silent unless they are an exception; the lock screen still hides the text.
  New `notify_ui.rs`.
- Saved Messages: a pinned row at the top of the chat list opens the chat with your own number
  (created locally on first use); the header says "Saved Messages" and the message menu gains
  "Save to Saved Messages" (forwards the message there). Hide it in Settings > Chats > "Show Saved
  Messages". New `saved_ui.rs`.
- Appearance (Settings > Chats > Appearance): Theme System / Light / Dark / Scheduled (dark from
  HH:MM to HH:MM, default 22:00-07:00, switches live; the default stays Dark), eight accent colours
  (WhatsApp green, blue, purple, pink, red, orange, teal, graphite) applied to buttons, badges, the
  selected tab and the outgoing bubble (its tint is computed per mode for readable text).
- Chat wallpapers: None (default), eight light/dark-aware presets or your own image (copied into the
  profile folder, scaled to at most 2560 px JPEG, cropped to fill, dimmed 0-60% in dark mode), for all
  chats (Settings) or one chat (chat header menu > Wallpaper..., with "Use default"). New
  `appearance_ui.rs`, `appearance.slint`.
- Photo viewer: clicking a photo opens an in-app viewer instead of the external program. Zoom with the
  mouse wheel (at the cursor), Ctrl+= / Ctrl+-, + / -, 0 = fit, double-click (fit / 100% or 2x), drag to
  pan (zoom range fit to 8x); rotate with R or the button (the file is untouched; large images are turned off the UI thread); browse all photos and
  videos of the chat with Left / Right or the on-screen arrows ("12 / 87"; photos download on demand).
  Top bar: sender and date, Copy image, Forward, Save as, Show in chat. Videos show their thumbnail with
  an "Open video" button (system player, as before). New `media_viewer.rs`, `media_viewer.slint`.
- Export chat (chat header menu): HTML (self-contained, light/dark), WhatsApp-style text or JSON,
  date range, optional copy of downloaded media, progress with Cancel. New `export/`, `export_ui.rs`,
  `export.slint`; store read helpers `export_count` / `export_page`.

### Fixed

- Composer keyboard: Ctrl+A/C/V/X/Z/Y and the other Ctrl shortcuts now work on non-Latin keyboard layouts (Persian, Arabic, Russian, Hebrew); Ctrl+X / Shift+Delete cut; Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z undo and redo (also formatting, mentions, paste, emoji and translation); Home / End in Persian lines go to the logical start / end.

## 0.8.0

### Added
- Emoji-only messages (1-3 emoji, ZWJ/flags/skin tones count as one) are shown large (48 / 40 / 32 px)
  without a bubble; the time sits in a small pill. Replies, media, links and locations keep the bubble.
- Voice notes: a pill on a playing/paused voice bubble cycles the speed 1x / 1.5x / 2x for the session
  (resampling, so the pitch rises with the speed; progress and time stay in note time).
- Storage manager (Settings > Storage): disk used by the profile (database, media, avatars), a bar
  split by type, and the top 50 chats by media size, each with "Clear media". "Clear cache…" clears
  by type and age ("older than 7 days / 30 days / 3 months / 1 year") and shows what it will free.
  Messages are never deleted; starred messages and unsent media are kept; cleared media can be
  downloaded again while it is on WhatsApp's servers. New `storage_ui.rs`, `storage.slint`, `wa_store::storage`.
- Message translation: "Translate" in the message menu shows the translation under the bubble
  ("Show original" toggles, errors offer Retry). Settings > Chats > Translation: target language and
  Manual (default) / Automatic mode; the language is asked on first use. Text goes to Google Translate.
  Cached translations of a chat are fetched when it opens. New `translate_ui.rs`, `translate.slint`.
- Translate before sending: a translate button in the composer (when it has text) translates the
  typed text into the chat's language, detected from the other side's recent messages, and puts
  the result back into the composer; a strip above it offers Undo. The chevron or a right-click
  picks the target language (remembered per chat). Never sent automatically. New `compose_translate.rs`.
- Search filters in the chat list: chips for Photos & videos, Links, Documents and Voice, a "From"
  sender picker and a date range (today, last 7 / 30 days, custom). Results are message hits from
  every chat with chat name, date and a thumbnail or icon; a click opens the chat at that message.
  Filters work without a query. New `global_search.rs`, `search_filters.slint`; store migration 12
  (indexes for newest-first, per-sender and per-kind scans) and `Store::search_everywhere`.
- Jump to date: a calendar button in the in-chat search bar opens a month calendar (days with
  messages are marked); picking a day scrolls to its first message, loading older history as needed.
  New `calendar.slint`.

### Fixed
- "Clear all media cache" no longer deletes starred messages' media or files still being sent; it
  uses the same safe planner as the storage manager.
- Translation fails fast (with "Can't reach the translation service. Check your connection or
  proxy.") when the network or proxy is down, manual requests no longer wait behind automatic ones,
  translations for a previous target language are not shown, and an edited message drops its old
  translation.

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
