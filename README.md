<div align="center">

<img src="src-tauri/icons/128x128.png" width="96" alt="Modpack Manager icon" />

# Modpack Manager

**A fast, lightweight desktop utility for building, loading, and LAN-syncing Minecraft modpacks.**

[![Latest release](https://img.shields.io/github/v/release/Kevfied/modpack-manager?color=white&label=release)](https://github.com/Kevfied/modpack-manager/releases/latest)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-white)](#installation)
[![Built with Tauri](https://img.shields.io/badge/tauri-v2-white)](https://v2.tauri.app)
[![Stack](https://img.shields.io/badge/stack-Rust%20%2B%20React%20%2B%20TS-white)](#tech-stack)

</div>

---

## What it does

Modpack Manager turns a folder of `.jar` files — or a hand-picked set of them — into a shareable Minecraft modpack, installs packs into your launcher, and keeps a group of friends on the same mods over LAN or Radmin VPN. No accounts, no cloud, no launcher required to build.

- **Scan** a whole mods folder recursively and/or pick individual jars — deduplicated, selectable, removable
- **Resolve** mod metadata and versions through Modrinth
- **Build** `.mrpack`, `.zip`, instance directories, and `modlist.txt`
- **Load** packs into detected launchers — without launching anything automatically
- **Connect** — host a live mod folder over LAN/Radmin and let friends sync it, with strict manual publishing
- **Update the app itself over LAN** — signed, verified, peer-reseeding updates so everyone stays current even when the original host is offline

The UI is a compact, monochrome, terminal-style window — sharp edges, no clutter. Tab order: **Loader · Mods · Build · Connect**.

---

## Installation

> **Just here to use the app?** Read the [**User Manual**](USER-MANUAL.md) — step-by-step for players, no technical knowledge needed.

Grab the latest installer from [**Releases**](https://github.com/Kevfied/modpack-manager/releases/latest):

| File | Notes |
|---|---|
| `Modpack Manager_X.X.X_x64-setup.exe` | **Recommended** — NSIS installer |
| `Modpack Manager_X.X.X_x64_en-US.msi` | Alternative MSI |

### Heads-up during install

- **Windows SmartScreen** will flag the unsigned installer → click **"More info" → "Run anyway"**. This is normal for unsigned builds.
- If your antivirus has **ransomware / controlled-folder protection** (Bitdefender enables it by default), it may block the app from writing synced packs into protected folders like Documents or Desktop — allow the app when prompted.
- Your firewall may ask to allow the app on **port 47676** the first time you host a Connect network or seed an update. That's the only port it ever listens on, and only when there's something to serve.

### Detected launchers

| Launcher | Notes |
|---|---|
| Minecraft Launcher (`.minecraft`) | Official launcher; also detects TLauncher profiles |
| TLauncher | Via `TlauncherProfiles.json` |
| Prism Launcher | Standard install paths |

Loading a pack never starts Minecraft or opens the launcher — it stages everything, you play when you want.

---

## Tabs

### Loader
The default tab. Drop in a `.mrpack` or `.zip` pack (or a folder) and install it into any detected launcher. Works completely standalone — no mod scan required. Duplicate instance names get an explicit overwrite prompt.

### Mods
Scan a mods folder recursively and add individually-picked jars in the same list — deduplicated by path. Select, exclude, remove, or clear; inspect jar metadata; resolve names/versions via Modrinth. Scanning runs in parallel so big folders don't freeze the UI.

### Build
Turn the selected mods into a pack:

| Output | Contents |
|---|---|
| `.mrpack` | Standard Modrinth pack — shareable, loadable |
| `.zip` | Plain archive of the selected jars |
| Instance directory | Ready-to-run folder structure |
| `modlist.txt` | Simple list of what's inside |

### Connect
LAN/Radmin VPN modpack synchronization.

- **Host shares a living folder** of `.jar` files — add or remove mods in the folder anytime, then push changes
- **Strict manual publish** — nothing is ever shared automatically. Mods only go out when *you* press publish. No auto-publish whatsoever.
- **Join codes** — require a code to join, or run an open network; network name and code are editable live
- **Targeted publish** — send to everyone or a single member/IP
- **Host controls membership** — only the host can kick; kicked members can rejoin with the code (kick ≠ ban)
- **Member table** shows each member's app version, so you can see who's behind
- Sessions survive code rotation — rotating the code only gates *new* joins

```
host:    mods folder ──publish──▶ served manifest + files (port 47676)
member:  discover ──join──▶ fetch manifest ──diff──▶ download only what changed
```

### App updates over LAN
The app updates itself peer-to-peer — independent of Connect networks and join codes. One command publishes; every running copy on the LAN finds it.

- **Signed manifests** — the publisher signs `latest.json` with a minisign key; every client verifies version, channel, architecture, and payload SHA-256 *before* offering anything
- **Explicit everywhere** — building an exe advertises nothing; installing an update always needs a click. Two "are you sure" gates, by design
- **Gossip-style reseeding** — after a peer verifies a download, it becomes a seeder too. Alice gets v1.4, the dev goes offline, Bob arrives later and downloads it *from Alice*
- **Resilient downloads** — `.part` staging, HTTP Range resume, failover across up to 3 seeders, free-space preflight, atomic rename only after verification
- **Can't be poisoned** — forged manifests, wrong keys, downgrades, replayed versions, and unsafe filenames are all rejected; the signature is the trust, not the sender

```
you:    release.ps1 → build → sign → "publish v1.4.0 to LAN? y" → seeding
alice:  hears announce → downloads from you → verifies sig → "Restart & install"
bob:    comes online later → hears alice's announce → downloads from her → same sig
```

---

## Security model

| Layer | Mechanism |
|---|---|
| Modpack sync | Join codes (host-set), host-only kick, manual publish only |
| Update trust | Detached **minisign signature on the manifest** — payload bound via SHA-256 inside it |
| Version safety | Strict semver-newer; downgrades, replays, and skipped versions rejected |
| Transport safety | Per-request threads (large transfers can't starve heartbeats), 64 KiB manifest cap, path-safe filenames |
| Install safety | `.part` staging + hash verify + atomic rename; running exe backed up to `updates-dl/prev` before install |
| Exposure | HTTP server **lazy-binds** — the port opens only while hosting or seeding |

Data lives in `%LOCALAPPDATA%\modpack-manager\` (networks, instances, update state). Installers never touch it — updates keep your networks, member ID, and synced packs intact.

---

## For developers

### Prerequisites

- [Node.js](https://nodejs.org) (LTS) + npm
- [Rust](https://rustup.rs) (stable)
- Windows: Microsoft C++ Build Tools; WebView2 (present on Win10/11 by default)

### Run / build

```powershell
npm install

npm run tauri dev      # hot-reload dev app
npm run tauri build    # production build → src-tauri/target/release/bundle/
```

### Testing

```powershell
cd src-tauri

cargo test --lib                          # unit tests
cargo run --example sandbox --release     # full feature matrix — must print 0 failure(s)
cargo run --example updstress --release   # dedicated updater stress suite (~85 checks)
npx tsc --noEmit                          # from repo root — frontend typecheck
```

Both sandboxes run fully isolated in temp directories and delete every artifact afterward — nothing leaks into real app data.

### Shipping an update

```powershell
.\scripts\release.ps1 -Bump patch    # bump version, build, sign, stage
# → "publish vX.Y.Z to LAN? y"       # only this makes it visible to peers
```

| Flag | Effect |
|---|---|
| `-Bump patch` | Increment the patch version in `tauri.conf.json` + `package.json` before building |
| `-SkipBuild` | Reuse an existing bundle output |
| `-BundleDir <path>` | Use an installer from a custom location |

The script signs the manifest with the key at `%LOCALAPPDATA%\modpack-manager\update-sign.key` (generated on first run — **never commit or distribute it**), uses a `minisign` CLI if installed, and falls back to the in-repo `relsign` example otherwise. Same-version republishing is refused unless forced — two different binaries can't pretend to be the same release.

---

## Tech stack

| Layer | Tech |
|---|---|
| Shell | Tauri v2 (Windows, custom title bar, small utility window) |
| Frontend | React 19 + TypeScript + Vite — monochrome dark UI |
| Backend | Rust — `tiny_http` LAN server, `reqwest` downloads, `minisign-verify` signatures, `rayon` parallel scanning, `zip`, `socket2` UDP discovery |
| Ports | `47676/tcp` HTTP (lazy-bound) · `47677/udp` discovery (`MMPK1` / `MMPKAPP`) |

### Project layout

```
├── src/                     # React frontend
│   ├── views/               # Loader · Mods · Build · Connect
│   ├── lib/api.ts           # Tauri command wrappers + types
│   └── styles.css           # monochrome theme
├── src-tauri/
│   ├── src/
│   │   ├── sync.rs          # LAN server, Connect protocol, update routing
│   │   ├── appupdate.rs     # signed update manifests, downloads, reseeding
│   │   ├── scanner.rs       # parallel jar scanning + metadata
│   │   ├── pack.rs          # .mrpack / .zip / instance building
│   │   ├── launchers.rs     # launcher detection + pack loading
│   │   └── modrinth.rs      # Modrinth API client
│   ├── examples/
│   │   ├── sandbox.rs       # full feature-matrix test (self-cleaning)
│   │   ├── updstress.rs     # updater stress suite (self-cleaning)
│   │   └── relsign.rs       # fallback manifest signer
│   └── icons/
└── scripts/
    └── release.ps1          # build → sign → stage → publish gate
```

---

## FAQ

**Does it launch Minecraft for me?**
No. It installs packs into launchers and stages everything — starting the game is always your call.

**Do my friends need the repo / Rust / Node?**
No — just the installer from Releases.

**Can updates install without me clicking?**
No. Peers can auto-*discover*, auto-*download*, and auto-*verify* — but installing always requires an explicit "Restart & install". Same for publishing: nothing goes out without the publish confirmation.

**What if the update server/host goes offline mid-download?**
The download resumes from the `.part` file against another seeder — up to 3 are tracked. If every seeder vanishes, the app shows "waiting for a seeder" and picks up where it left off the moment anyone re-announces.

**I skipped a version — am I stuck?**
No. Skips are per-version; the next release prompts again.

**What data does an update keep?**
Everything in `%LOCALAPPDATA%\modpack-manager\` — networks, member identity, synced packs, applied versions. The installer only replaces program files.
