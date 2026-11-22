# WaClient — handoff notes (state as of 2026-11-22, v0.9.0)

Read this first in a new session. It records what exists, what was decided and why,
what is untested, and how to work on the project safely. README.md is the user-facing
feature list; CHANGELOG.md is the per-version history.

## 1. What this is

A fast native Windows WhatsApp client (replacement for the slow official app), written in
Rust. English UI. The owner tests on a secondary number and accepts ban risk, but wants the
client to look as much like the official one as possible.

- Workspace: `crates/store` (`wa_store`, rusqlite 0.39 bundled), `crates/engine`
  (`wa_engine`, whatsapp-rust 0.7, `default-features=false` minus `simd`), `app` (`waclient`,
  Slint 1.19 UI). Version lives in root `Cargo.toml` (`version = "0.9.0"`).
- Latest local package: `dist/WaClient-0.6.0-win-x64.zip` (keep only the latest release in `dist/`).
- Identity: `ClientProfile::web()` (the Windows profile was rejected by the server as
  "outdated"), DevicePropsOverride os "Windows", PlatformType UWP, require_full_sync,
  365-day history.

## 2. Commands

| What | Command |
|---|---|
| Build (debug) | `cargo build -p waclient` |
| Test all | `cargo test --workspace` (1103 passing at v0.9.0, 3 ignored) |
| Test one area | `cargo test -p wa_engine <filter>` / `cargo test -p waclient <filter>` |
| Engine+store only | `cargo build -p wa_engine -p wa_store` |
| Store perf smoke | `cargo test -p wa_store --test perf -- --ignored` |
| Package | `powershell -File scripts\package.ps1` (`-DryRun` to preview; `-Installer` needs Inno Setup) |
| Demo UI (no account) | `WACLIENT_DEMO=1 SLINT_BACKEND=winit-software` + `WACLIENT_DEMO_UI=<action>` |

- `.cargo/config.toml` sets `CMAKE` to the VS 2026 cmake path.
- Dev profile has reduced debug info (disk). Release: thin LTO, codegen-units 1, strip.
- Packaging fails if the user's `WaClient.exe` is running (file lock). Ask them to quit
  from the tray first. The single-instance mutex also kills demo runs while their app is open.

## 2b. Git, GitHub and releases

- Repo: https://github.com/nRafinia/WaClient (PUBLIC). Scan staged files for private data before
  every push (no *.db, applock.json, proxy.json, folders.json, emails, personal paths, tokens).
- `.github/workflows/release.yml`: every push/merge to `main` runs `cargo test --workspace` on
  windows-latest, packages with `scripts/package.ps1 -Installer` (Inno Setup via choco), and creates
  a GitHub release: `v<version>` if that tag is new (notes = the matching CHANGELOG section), else a
  pre-release `v<version>-build.<run>`. Bump `[workspace.package] version` + rename the CHANGELOG
  "Unreleased" heading before merging a real release. CI sets `CMAKE=cmake` (the local
  `.cargo/config.toml` path is this PC's Visual Studio).

## 3. Hard rules (privacy / safety) — keep honouring these

- Never run tests/demos against the real `%APPDATA%\WaClient` data. Demo mode uses its own store.
- Screenshots: capture ONLY the WaClient window (PrintWindow script in the session scratchpad,
  `cap.ps1 '<demo-ui>' '<out.png>'`). An earlier full-screen capture leaked another window.
- Logs at info and above: no JIDs (use `jid::redact` / `models::redact_jid`), no phone
  numbers, no message/status text, no coordinates (debug only, rounded to 2 decimals).
- No `unwrap`/`expect` outside tests.
- Don't commit or push unless asked.
- User's email is private; never send it to external services.
- Disk is small (drive E:, ~150 GB): after each batch of agents, delete
  `target/debug/incremental`, stale `target/debug/deps/*.exe|*.pdb`, `target/release` after
  packaging, and old `dist/` versions. Never `cargo clean` while agents are building.

## 4. Architecture essentials

- Engine/UI contract: `crates/engine/src/api.rs` — `Command` enum (UI → engine),
  `UiEvent` enum (engine → UI), `EngineHandle`, `start_with_settings`. Every new command
  needs: api.rs variant + `log_line` arm, `commands.rs` dispatch, `runtime.rs` routing,
  `app/src/demo.rs` `command_name` arm, and a `UiEvent` arm in `app/src/app.rs` if it replies.
- Routing (`runtime.rs`): ordered send worker (sends, forwards, contact/location/status
  posts), ctl worker (edit, pin, timers), flags worker (archive/pin/mute/star), default spawn.
- Shared chat send path: `commands::store_pending` + `commands::deliver`. `deliver` applies
  the chat's disappearing timer; anything sent to a chat must go through both. Statuses use
  the library status API directly (never get a timer).
- Store migrations: `crates/store/src/lib.rs` `MIGRATIONS` (10 at v0.4.0). `MESSAGE_COLS`
  has 28 columns; hard-coded column indexes exist (e.g. `social.rs` viewed = 28) — update
  them whenever columns are added.
- Multi-profile: `profiles.json` registry (crash-safe journaled migration), per-profile data
  dir, all profiles online at once. Storage location is movable (copy → verify → atomic
  record → delete old), see `app/src/location.rs` (that file is about the DATA folder;
  geolocation is `app/src/geo.rs`).
- Slint rule (past bug): in PopupWindow menus invoke the callback BEFORE closing the popup.
- Esc layering lives in `app/src/app/esc.rs` (priority test must be updated for new layers).
- Icons: Path-drawn components in `app/ui/icons.slint` — font glyphs like ✕ ✓ ← render as
  boxes on Windows; don't use them as icons.

## 5. Decisions already made (don't re-litigate)

- Received view-once media is never stored/viewable/forwardable here: placeholder
  "🔒 View once message — open it on your phone"; quotes of view-once are stripped too.
- Mic stall during recording: never auto-send — recording stops, file kept, user chooses
  Send/Cancel.
- Edit after 15 min / delete-for-everyone after ~2 days: impossible (server-enforced); show a
  clear error.
- Read receipts privacy: global switch + per-chat override + 👻 quick toggle; "Mark as read
  now" is the only override. Statuses viewable without "seen" (800 ms dwell when allowed).
- Status audience: the phone's status-privacy setting can't be read by the library →
  "My contacts" = address-book contacts (`contacts.full_name` non-blank, set only by phone
  contact sync), plus per-post "Except…" / "Only share with…", last choice remembered per
  profile. Blocked contacts are always excluded (both PN and LID forms).
- Phone lookups (ban risk): max 20 numbers/command, 60 uncached per 10 min, 300 per 24 h,
  2 s spacing, 10-min cache; numbers starting with 0 are rejected (UI adds own country code).
- Disappearing messages: per-chat timer + account default; WhatsApp has no per-message
  timer → per-message = "send as view once" (photo/video). Expired messages are deleted
  locally with their media unless starred (purger every 60 s).
- Location: "Send my current location" via Windows Location API (needs Windows Settings ›
  Privacy › Location for desktop apps) or pasted coordinates / Google / OSM / Apple links.
  Short `goo.gl` links unsupported (no network resolution). Live location: view only.
- Calls: tier 1 only (decided 2026-11-21) — incoming-call banner, Decline
  (`client.voip().reject`, ungated), `CALL-{call_id}` rows ("Missed …" unread, "Declined …" not
  unread), module `calls.rs`. The ringing entry is taken out of the map before the reject is
  sent; ringing calls survive Disconnected (2-min TTL), cleared on logout / stream replaced.
  Answering/placing calls (whatsapp-rust `voip-*` features: webrtc, opus 0.3, 16 kHz 60 ms
  streaming audio) and video (needs an H.264 codec) were judged heavy and risky; not started.
- Keyboard shortcuts are layout-independent (`app/src/layout_keys.rs`, hooked in drop_paste.rs's
  single `on_winit_window_event`): with Ctrl held (no Alt), a physical letter/digit/period key whose
  character is NOT ASCII (Persian, Arabic, Cyrillic, Hebrew layouts) is re-dispatched as the US
  letter. ASCII layouts (QWERTY/QWERTZ/AZERTY/Dvorak) are never touched. Any future
  `on_winit_window_event` user must keep calling `layout_keys::remap`.
- Composer undo/redo is our own stack (`app/src/app/undo.rs`): Slint clears its native undo on every
  programmatic `text =` (formatting, mentions, emoji, paste, translate). RTL paragraphs get logical
  Home/End.
- Notify carries `muted: bool`; the engine emits for muted chats and the app decides ("always notify"
  exception, sounds, lock masking) in `notify_ui.rs`.
- Send later (`schedule_ui.rs`, per-profile `scheduled.json`): remove-then-send (never double-send),
  overdue items are held for the user, never sent silently after the app was closed.
- Export (`app/src/export/`): always writes into a folder it creates (or an empty one); never
  overwrites; HTML escapes everything; only http/https/mailto links.
- Light theme: colours must come from Theme tokens (theme.slint has light+dark values); don't add
  hard-coded surface colours in .slint files.
- Live-location SENDING is deliberately not built (decided 2026-11-21): whatsapp-rust 0.7 has
  no protocol-correct way to send position updates or stop a share (only the initial
  LiveLocationMessage), and the official Windows/Web clients can't share live location either,
  so doing it from a "Windows" device would look anomalous (ban risk). Viewing stays.
- Own text status rows store style in `media_mime = "text/status;bg=AARRGGBB;font=N"`.
- Outgoing messages sent from this app are stored with ts = max(local clock, newest row in the
  chat within +24h) (`Store::insert_outgoing`), so a slow PC clock can't sort our reply above
  the message it answers (user-reported bug, 0.5.0).
- Polls: our own vote is stored optimistically under `own_jid` (PN) and reverted on failure.
  Created polls are built manually (V3 single answer, V1 selectable 0 for multi, 32-byte
  messageSecret) and sent via `store_pending` + `deliver`. The vote's creator JID is sent in
  the namespace of the chat (LID groups → LID) — see `polls::creator_form`.
- Groups: module `group_admin.rs`; remove/promote/demote translate PN → LID in LID-addressed
  groups; per-member results with plain-English errors (403 on add = privacy → offer to send
  the invite link, max 20 people per action). Create/Join failures have their own events
  (`GroupCreateFailed`, `GroupJoinFailed`) so unrelated errors can't re-enable the dialog.
  Group-change notices are rows with id `GRP-{notification_id}-{action_index}` (no migration).
  Rights-changing updates (announce, our remove/promote/demote) trigger `info::fetch` so the
  composer gate ("Only admins can send messages") refreshes.
- Modal flows (pickers, New group, Create poll) close on profile switch and re-check the
  profile before sending.

## 6. Untested on a real account / real hardware (ask the user to verify)

- 0.6.0: incoming calls — the action sequence on caller hang-up (Terminate
  reasons), whether the server accepts our reject, offline MissedCall rows (no voice/video info),
  taskbar flash.
- 0.5.0: everything poll/group on the server side: vote acceptance (incl. withdraw = empty
  vote, multi-answer V1 polls with selectable 0, votes on our own polls in LID groups — the
  library sets from_me=false there), create group (are privacy-blocked members omitted from
  the reply?), error codes 401/403/404/409 per member, PN vs LID for member changes, "not a
  member" detection (401/403/404 on metadata), invite preview/join/pending approval, whether
  the server sends group notifications for our own changes, the timestamp-clamp fix live.
- 0.4.0: Windows geolocation; real status posting and audience; chat timers syncing with the
  phone; view-once sending; phone-number lookup; contact sending as seen by recipients.
- 0.3.0: right-click opening the message menu (the ⌄ arrow works); profile drag-reorder;
  Ctrl+1..5; mic unplug during recording; real voice upload.
- Message info: receipts received before the 0.4.0 fix were lost (only status shown).

## 7. Known limitations / ideas for next rounds

- **Next work is planned in `docs/ROADMAP.md`** (Telegram-inspired features, phased and
  prioritised, agreed 2026-11-21). Start with Phase 0 (live-test feedback), then Phase 1.

- Not built: voice/video calls (library support only tested on Linux), live-location sending,
  installer (script ready; needs Inno Setup installed), quizzes, per-voter "View votes",
  approving membership requests, communities.
- Rare edge cases accepted in 0.5.0: two group notices in the same second without a
  notification id → second dropped; reverting a failed vote can restore an older vote that was
  withdrawn on another device meanwhile; forwarding is not blocked in admins-only groups; the
  info panel / composer gate may be stale up to 5 min (metadata cache).

## 8. How the work was organised (worked well)

- The lead plans, writes the API/schema scaffold first (placeholder arms in commands.rs,
  one per feature area, so parallel engine agents don't collide), then fans out engine and
  UI agents with disjoint "only touch" file lists, then runs read-only reviewers in parallel
  with the next writers, then an integration pass, then package + disk cleanup.
- Agents share one `target/`: concurrent builds hit file locks ("unable to copy .o") — retry.
- Demo screenshots: the demo single-instance name is shared, so concurrent demos kill each
  other — retry. `seq:chat;open:111` opens a chat; clicks are client pixels.
- The user writes in Persian and repeatedly asks to add parallel agents; only parallelise
  disjoint files and say plainly when more agents would just collide.
