# Modpack Manager

Tauri v2 desktop app — Rust backend (`src-tauri`), React+TS frontend (`src`), Vite.

## Verify

- `cd src-tauri && cargo test --lib` — unit tests
- `npx tsc --noEmit` — frontend typecheck
- `cd src-tauri && cargo run --example sandbox --release` — full feature matrix
  (scan/build/load/connect + app-update); must end with **0 failure(s)** and
  self-cleans its temp dir.

## Connect (LAN modpack sync)

`src-tauri/src/sync.rs`. HTTP on 47676, UDP discovery on 47677 (`MMPK1`
announce, `MMPKQ` member query → host replies with instant unicast announce,
`MMPKAPP` update-seed announce). Found networks persist in `connect.json`
(`Store.found`) and resurface as stale entries (`live: false`, 14-day TTL).
Strict rule: nothing is published/applied without an explicit user action.

## App updates (LAN, standalone — not Connect)

`src-tauri/src/appupdate.rs` + `scripts/release.ps1`.

- Trust = detached **minisign signature on the manifest** (`latest.json.minisig`),
  not on who serves it. Payload is bound by sha256 inside the signed manifest.
- `UPDATE_PUBKEY` in `appupdate.rs` is a placeholder until a real key is
  generated — `release.ps1` prints it on first run.
- HTTP server (`sync.rs`) binds lazily only when there's something to serve:
  Connect hosting or a seeded update bundle. `/app/*` is unauthenticated.
- `runningVersion` (the exe) vs `bundleVersion` (the published seed) are
  separate — building/running a new exe never advertises it.
- Purge rule: `downloaded` purges at <= running version, `seed` only at
  < running — never purge the version friends still need.
- App data lives in `%LOCALAPPDATA%\modpack-manager\` — installers never
  touch it. All persisted structs use `#[serde(default)]`.

## Conventions

- One way to turn "folder of jars + pack meta" into a playable launcher
  entry: `launchers::deploy_mods_dir` (mirror-copy jars, prune stale,
  resolve loader version). Connect member apply, host apply
  (`load_host_pack`), and Loader-folder loads all funnel through it —
  don't fork the logic.
- Monochrome dark UI, sharp edges, monospace; tab order Loader | Mods | Build
  | Connect (Connect default — members live there).
- Async Tauri commands; no std MutexGuard held across `.await`.
- HTTP handlers are spawned per-request — never block them.
