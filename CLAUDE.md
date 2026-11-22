# WaClient (Rust WhatsApp client for Windows)

Start every session by reading `docs/HANDOFF.md` — current state, decisions, rules, untested
items and next ideas. README.md = features; CHANGELOG.md = history.

## Project commands

- Build: `cargo build -p waclient`
- Test (all): `cargo test --workspace`
- Test (single file / filter): `cargo test -p wa_engine <filter>` / `cargo test -p waclient <filter>`
- Lint / format check: zero compiler warnings is the bar (no clippy gate configured)
- Run locally: `cargo run -p waclient` (demo without an account: `WACLIENT_DEMO=1`)
- Package: `powershell -File scripts\package.ps1`

## Must-follow

- Never touch the real `%APPDATA%\WaClient` in tests/demos; screenshot only the WaClient window.
- No JIDs, phone numbers, message text or coordinates in logs at info+.
- Clean `target/` leftovers after each batch of agents; keep only the latest `dist/` release.
- No commits unless the user asks.
