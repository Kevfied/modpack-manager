//! LAN app updates — any instance can seed a signed update bundle; every
//! instance discovers seeders over UDP and pulls the installer. Trust comes
//! from a detached minisign signature on the manifest, not from who serves it.
//! Independent of Connect networks: no codes, no membership.
//!
//! Strict rule: nothing installs without an explicit user action; nothing
//! advertises without a deliberate publish (release.ps1 or bundle pick).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashMap;
use std::io::Read;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sync;

/// The developer's update public key — the ONLY trust root for LAN updates.
/// Private half lives in %LOCALAPPDATA%\modpack-manager\update-sign.key
/// (never committed). Regenerate + redistribute the app if it's ever lost.
pub const UPDATE_PUBKEY: &str = "RWQJG5cGQiOZYmb27ThhaQgxndhzSnJokJadxcz0CYG1H1n+xMUPBLAr";
const APP_ID: &str = "com.modpackmanager.app";
const CHANNEL: &str = "stable";
const MANIFEST_CAP: usize = 64 * 1024;
const ANNOUNCE_SECS: u64 = 3;
const SEEDER_TTL_SECS: u64 = 12;
const MAX_SEEDERS: usize = 3;

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub app: String,
    pub version: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default, rename = "pubDate")]
    pub pub_date: String,
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FoundSeeder {
    pub addr: String,
    #[serde(rename = "bundleVersion")]
    pub bundle_version: String,
    #[serde(rename = "runningVersion")]
    pub running_version: String,
    pub machine: String,
    #[serde(skip)]
    pub seen: u64,
}

/// What the UI renders. `state`: idle | offer | downloading | downloaded |
/// waiting | skipped.
#[derive(Debug, Clone, Serialize)]
pub struct AppUpdateStatus {
    pub state: String,
    pub version: String,
    pub notes: String,
    pub size: u64,
    pub source: String,
    pub progress: f64,
    #[serde(rename = "runningVersion")]
    pub running_version: String,
    /// Set while a "✓ updated" welcome is pending dismissal (the new version).
    #[serde(rename = "justUpdated")]
    pub just_updated: String,
    /// Release notes captured at install time for the justUpdated banner.
    #[serde(rename = "updatedNotes")]
    pub updated_notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DownloadedEntry {
    pub version: String,
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct AppUpdateStore {
    #[serde(rename = "skippedVersion")]
    skipped_version: Option<String>,
    #[serde(rename = "knownVersion")]
    known_version: Option<String>,
    downloaded: Option<DownloadedEntry>,
    /// Version the app was running at last status check — detects upgrades.
    #[serde(default, rename = "lastRun")]
    last_run: Option<String>,
    /// New version awaiting the post-update banner's dismissal.
    #[serde(default, rename = "welcomePending")]
    welcome_pending: Option<String>,
    /// Notes for the version most recently installed via the updater.
    #[serde(default, rename = "installedNotes")]
    installed_notes: String,
}

#[derive(Default)]
struct DownloadState {
    running: bool,
    cancel: bool,
    done: u64,
    total: u64,
}

static APP_DIR: OnceLock<PathBuf> = OnceLock::new();
static FOUND_UPDATES: OnceLock<Mutex<HashMap<String, FoundSeeder>>> = OnceLock::new();
static DL: OnceLock<Mutex<DownloadState>> = OnceLock::new();

fn dl_state() -> &'static Mutex<DownloadState> {
    DL.get_or_init(|| Mutex::new(DownloadState::default()))
}

fn found_map() -> &'static Mutex<HashMap<String, FoundSeeder>> {
    FOUND_UPDATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------- paths

/// Called once at app startup (and by tests) — anchors all update dirs.
pub fn init(app_dir: &Path) {
    let _ = APP_DIR.set(app_dir.to_path_buf());
}

fn app_dir() -> PathBuf {
    APP_DIR
        .get()
        .cloned()
        .or_else(|| sync::store_path().parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn seed_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("updates-seed")
}

pub fn dl_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("updates-dl")
}

fn store_file(app_dir: &Path) -> PathBuf {
    app_dir.join("appupdate.json")
}

fn running_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn running_semver() -> semver::Version {
    semver::Version::parse(&running_version()).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

// ---------------------------------------------------------------- store

fn load_store(app_dir: &Path) -> AppUpdateStore {
    std::fs::read_to_string(store_file(app_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_store(app_dir: &Path, s: &AppUpdateStore) {
    if let Ok(j) = serde_json::to_string_pretty(s) {
        let f = store_file(app_dir);
        if let Some(p) = f.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(f, j);
    }
}

pub fn skip_version(app_dir: &Path, version: &str) {
    let mut s = load_store(app_dir);
    s.skipped_version = Some(version.to_string());
    save_store(app_dir, &s);
}

pub fn unskip_version(app_dir: &Path) {
    let mut s = load_store(app_dir);
    s.skipped_version = None;
    save_store(app_dir, &s);
}

// ---------------------------------------------------------------- manifest gate

fn bare_filename_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains("..")
}

/// Everything checked before an offer is even shown. `current`/`skipped`
/// parameters make this testable.
pub fn gate_manifest(m: &UpdateManifest, current: &str, skipped: Option<&str>) -> Result<()> {
    if m.app != APP_ID {
        anyhow::bail!("manifest is for a different app");
    }
    if !m.channel.is_empty() && m.channel != CHANNEL {
        anyhow::bail!("wrong channel");
    }
    if !m.arch.is_empty() && m.arch != std::env::consts::ARCH {
        anyhow::bail!("wrong architecture");
    }
    let v = semver::Version::parse(&m.version).context("bad version in manifest")?;
    let cur = semver::Version::parse(current).unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    if v <= cur {
        anyhow::bail!("not newer");
    }
    if Some(m.version.as_str()) == skipped {
        anyhow::bail!("version skipped");
    }
    if !bare_filename_ok(&m.file) {
        anyhow::bail!("bad filename in manifest");
    }
    Ok(())
}

/// Verify the detached minisign signature over the raw manifest bytes.
pub fn verify_manifest_sig(
    manifest_bytes: &[u8],
    sig_text: &str,
    pubkey_b64: &str,
) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(pubkey_b64)
        .map_err(|e| anyhow::anyhow!("bad pubkey: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))?;
    pk.verify(manifest_bytes, &sig, false)
        .map_err(|e| anyhow::anyhow!("signature verify failed: {e}"))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

// ---------------------------------------------------------------- serving

/// Seeded manifest, if any (read fresh — cheap, small file).
fn seed_manifest(dir: &Path) -> Option<(UpdateManifest, Vec<u8>)> {
    let bytes = std::fs::read(seed_dir(dir).join("latest.json")).ok()?;
    let m: UpdateManifest = serde_json::from_slice(&bytes).ok()?;
    Some((m, bytes))
}

/// The version we can serve right now — used by the announce loop.
pub fn seed_bundle_version(dir: &Path) -> Option<String> {
    seed_manifest(dir).map(|(m, _)| m.version)
}

/// HTTP handler for /app/* — called from the shared server in sync.rs.
/// Unauthenticated by design: the signature is the trust anchor.
pub fn handle_app_request(req: tiny_http::Request, path: &str, query: &str) {
    let dir = app_dir();
    let seed = seed_dir(&dir);
    match path {
        "/app/latest.json" | "/app/latest.json.minisig" => {
            let name = path.trim_start_matches("/app/");
            serve_path(req, &seed.join(name), 0);
        }
        "/app/file" => {
            let name = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let name = url_decode(&name);
            let Some((m, _)) = seed_manifest(&dir) else {
                return respond_json(req, 404, &serde_json::json!({"error": "no bundle"}));
            };
            if name != m.file || !bare_filename_ok(&name) {
                return respond_json(req, 404, &serde_json::json!({"error": "not found"}));
            }
            // Optional Range support for crash-resume.
            let range_from = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .and_then(|h| h.value.as_str().strip_prefix("bytes="))
                .and_then(|s| s.split('-').next()?.parse::<u64>().ok());
            serve_path(req, &seed.join(&name), range_from.unwrap_or(0));
        }
        _ => respond_json(req, 404, &serde_json::json!({"error": "not found"})),
    }
}

fn respond_json(req: tiny_http::Request, code: u32, v: &serde_json::Value) {
    let body = serde_json::to_string(v).unwrap_or_default();
    let resp = tiny_http::Response::from_string(body)
        .with_status_code(tiny_http::StatusCode(code as u16))
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .unwrap(),
        );
    let _ = req.respond(resp);
}

fn serve_path(req: tiny_http::Request, path: &Path, range_from: u64) {
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return respond_json(req, 404, &serde_json::json!({"error": "not found"})),
    };
    let total = meta.len();
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return respond_json(req, 404, &serde_json::json!({"error": "not found"})),
    };
    if range_from >= total && total > 0 {
        return respond_json(req, 416, &serde_json::json!({"error": "bad range"}));
    }
    let mut headers = vec![
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..])
            .unwrap(),
        tiny_http::Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap(),
    ];
    if range_from > 0 {
        use std::io::Seek;
        if f.seek(std::io::SeekFrom::Start(range_from)).is_err() {
            return respond_json(req, 416, &serde_json::json!({"error": "bad range"}));
        }
        let len = total - range_from;
        headers.push(
            tiny_http::Header::from_bytes(
                &b"Content-Range"[..],
                format!("bytes {range_from}-{}/{total}", total - 1).as_bytes(),
            )
            .unwrap(),
        );
        let resp = tiny_http::Response::new(
            tiny_http::StatusCode(206),
            headers,
            f.take(len),
            Some(len as usize),
            None,
        );
        let _ = req.respond(resp);
    } else {
        let resp = tiny_http::Response::new(
            tiny_http::StatusCode(200),
            headers,
            f.take(total),
            Some(total as usize),
            None,
        );
        let _ = req.respond(resp);
    }
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- announce/listen

/// Advertise the seeded bundle. Started once; self-manages bind retries.
pub fn start_update_announcer(hs: sync::SharedHost, app_dir: PathBuf) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = sock.set_broadcast(true);
        loop {
            // Slowed — not stopped — while the window is trayed: peers
            // can still discover and pull the seeded bundle.
            std::thread::sleep(Duration::from_secs(
                if sync::APP_HIDDEN.load(std::sync::atomic::Ordering::Relaxed) {
                    15
                } else {
                    ANNOUNCE_SECS
                },
            ));
            let Some(version) = seed_bundle_version(&app_dir) else {
                continue; // nothing to serve — stay silent
            };
            // Lazy-bind + retry: the port only opens when we can actually serve.
            if !sync::ensure_server(&hs) {
                continue;
            }
            let msg = serde_json::json!({
                "magic": "MMPKAPP",
                "bundleVersion": version,
                "runningVersion": running_version(),
                "channel": CHANNEL,
                "port": sync::HTTP_PORT,
                "machine": machine_name(),
            })
            .to_string();
            for b in sync::broadcast_targets() {
                let _ = sock.send_to(msg.as_bytes(), (b, sync::BROADCAST_PORT));
            }
        }
    });
}

fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

/// Passive listener for MMPKAPP announces — always safe to run.
/// Fired when a seeder announce reveals something new — a new seeder or a
/// version bump — so the UI can re-check instantly rather than on its poll.
static SEED_HOOK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn set_seed_hook(f: impl Fn() + Send + Sync + 'static) {
    let _ = SEED_HOOK.set(Box::new(f));
}

/// There is exactly ONE UDP socket on the broadcast port — sync's listener
/// demuxes by `magic` and hands MMPKAPP datagrams here via
/// `note_seeder_announce`. A second socket would starve whichever bound
/// first: on Windows SO_REUSEADDR delivers each datagram to only one bound
/// socket, so two listeners on one port can never both hear traffic.
pub fn start_update_listener() {
    crate::sync::start_discovery_listener();
}

/// sync's UDP demux calls this for every MMPKAPP datagram received.
pub(crate) fn note_seeder_announce(v: &serde_json::Value, src: std::net::IpAddr) {
    let port = v["port"].as_u64().unwrap_or(sync::HTTP_PORT as u64);
    let f = FoundSeeder {
        addr: format!("{src}:{port}"),
        bundle_version: v["bundleVersion"].as_str().unwrap_or("").to_string(),
        running_version: v["runningVersion"].as_str().unwrap_or("").to_string(),
        machine: v["machine"].as_str().unwrap_or("?").to_string(),
        seen: now_secs(),
    };
    if f.bundle_version.is_empty() {
        return;
    }
    let mut m = found_map().lock().unwrap();
    let changed = m
        .get(&f.addr)
        .map(|o| o.bundle_version != f.bundle_version)
        .unwrap_or(true);
    let machine = f.machine.clone();
    let version = f.bundle_version.clone();
    let addr = f.addr.clone();
    m.insert(addr.clone(), f);
    drop(m);
    if changed {
        crate::alog!("upd", "seeder {machine} offers v{version} ({addr})");
        if let Some(h) = SEED_HOOK.get() {
            h();
        }
    }
}

/// Seeders seen recently, best version first.
pub fn found_seeders() -> Vec<FoundSeeder> {
    start_update_listener();
    let now = now_secs();
    let mut m = found_map().lock().unwrap();
    m.retain(|_, f| now.saturating_sub(f.seen) < SEEDER_TTL_SECS);
    let mut v: Vec<FoundSeeder> = m.values().cloned().collect();
    v.sort_by(|a, b| {
        let va = semver::Version::parse(&a.bundle_version)
            .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
        let vb = semver::Version::parse(&b.bundle_version)
            .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
        vb.cmp(&va)
    });
    v
}

/// Forget all discovered seeders — used by tests and on manual refresh.
pub fn clear_seeders() {
    found_map().lock().unwrap().clear();
}

/// Manual fallback — pretend a seeder was announced at this addr.
pub fn add_manual_seeder(addr: &str) {
    let addr = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{}", sync::HTTP_PORT)
    };
    found_map().lock().unwrap().insert(
        addr.clone(),
        FoundSeeder {
            addr,
            bundle_version: "999.0.0".into(), // manifest fetch decides the truth
            running_version: String::new(),
            machine: "manual".into(),
            seen: now_secs(),
        },
    );
}

// ---------------------------------------------------------------- fetch/verify

async fn fetch_capped(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut r = http
        .get(url)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?;
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() > MANIFEST_CAP {
            anyhow::bail!("response too large");
        }
    }
    Ok(out)
}

/// Fetch + sig-verify + gate a manifest from one seeder.
/// Returns (manifest, raw manifest bytes, sig text) — the bytes+sig are kept
/// for viral reseeding.
async fn verified_manifest(
    http: &reqwest::Client,
    addr: &str,
    skipped: Option<&str>,
    pubkey: &str,
) -> Result<(UpdateManifest, Vec<u8>, String)> {
    let bytes = fetch_capped(http, &format!("http://{addr}/app/latest.json")).await?;
    let sig_bytes = fetch_capped(http, &format!("http://{addr}/app/latest.json.minisig")).await?;
    let sig_text = String::from_utf8_lossy(&sig_bytes).into_owned();
    verify_manifest_sig(&bytes, &sig_text, pubkey)?;
    let m: UpdateManifest = serde_json::from_slice(&bytes).context("bad manifest json")?;
    gate_manifest(&m, &running_version(), skipped)?;
    Ok((m, bytes, sig_text))
}

// ---------------------------------------------------------------- status

pub async fn app_update_status(http: &reqwest::Client, dir: &Path) -> AppUpdateStatus {
    app_update_status_pk(http, dir, UPDATE_PUBKEY).await
}

/// Same as `app_update_status` but with an injectable key (tests).
pub async fn app_update_status_pk(
    http: &reqwest::Client,
    dir: &Path,
    pubkey: &str,
) -> AppUpdateStatus {
    let running = running_version();
    let mut store = load_store(dir);
    // Version changed since last run → a welcome banner is owed. Flagged
    // persistently so it's emitted until the user dismisses it.
    if store.last_run.as_deref() != Some(running.as_str()) {
        if store.last_run.is_some() {
            store.welcome_pending = Some(running.clone());
        }
        store.last_run = Some(running.clone());
        save_store(dir, &store);
    }
    let just_updated = store.welcome_pending.clone().unwrap_or_default();
    let updated_notes = if just_updated.is_empty() {
        String::new()
    } else {
        store.installed_notes.clone()
    };
    let base = |state: &str| AppUpdateStatus {
        state: state.into(),
        version: String::new(),
        notes: String::new(),
        size: 0,
        source: String::new(),
        progress: 0.0,
        running_version: running.clone(),
        just_updated: just_updated.clone(),
        updated_notes: updated_notes.clone(),
    };

    // Downloaded-and-verified bundle? Surfaces when nothing newer is being
    // announced — or when seeders are gone entirely (offline install).
    let downloaded_ver = store.downloaded.as_ref().and_then(|d| {
        let p = PathBuf::from(&d.path);
        let ok = p.is_file()
            && semver::Version::parse(&d.version)
                .map(|v| v > running_semver())
                .unwrap_or(false)
            && sha256_file(&p).map(|h| h == d.sha256).unwrap_or(false);
        ok.then(|| (d.version.clone(), d.notes.clone()))
    });

    let (dl_running, dl_progress) = {
        let st = dl_state().lock().unwrap();
        (
            st.running,
            if st.total > 0 {
                st.done as f64 / st.total as f64
            } else {
                0.0
            },
        )
    };
    if dl_running {
        let mut s = base("downloading");
        s.progress = dl_progress;
        return s;
    }

    // Best seeder = highest announced version that beats ours.
    let seeders = found_seeders();
    let best = seeders.iter().find(|f| {
        semver::Version::parse(&f.bundle_version)
            .map(|v| v > running_semver())
            .unwrap_or(false)
    });

    if let Some(f) = best {
        // An offer only displaces a staged download if it's strictly newer.
        let newer_than_dl = |bv: &str| -> bool {
            match (&downloaded_ver, semver::Version::parse(bv)) {
                (Some((dv, _)), Ok(bv)) => semver::Version::parse(dv)
                    .map(|d| bv > d)
                    .unwrap_or(false),
                (None, _) => true,
                _ => false,
            }
        };
        if newer_than_dl(&f.bundle_version) {
            // Skipped versions never even fetch a manifest.
            if store.skipped_version.as_deref() == Some(f.bundle_version.as_str()) {
                let mut s = base("skipped");
                s.version = f.bundle_version.clone();
                s.source = f.machine.clone();
                return s;
            }
            match verified_manifest(
                http,
                &f.addr,
                store.skipped_version.as_deref(),
                pubkey,
            )
            .await
            {
                Ok((m, _, _)) => {
                    // Announces can lie — double-check the served manifest
                    // really is newer than what we already staged.
                    if newer_than_dl(&m.version) {
                        if let Some(d) = &store.downloaded {
                            let _ = std::fs::remove_file(&d.path);
                            let mut s2 = load_store(dir);
                            s2.downloaded = None;
                            save_store(dir, &s2);
                        }
                        let mut s = base("offer");
                        s.version = m.version.clone();
                        s.notes = m.notes;
                        s.size = m.size;
                        s.source = f.machine.clone();
                        return s;
                    }
                }
                Err(e) => {
                    // Bad/forged/unreachable seeder — purge it, retry next poll.
                    found_map().lock().unwrap().remove(&f.addr);
                    log::warn!("update manifest from {} rejected: {e}", f.addr);
                }
            }
        }
    }

    if let Some((dv, dnotes)) = downloaded_ver {
        let mut s = base("downloaded");
        s.version = dv;
        s.notes = dnotes;
        s.progress = 1.0;
        return s;
    }

    // Waiting state: we know a newer version exists but nobody's up.
    let mut s = base("idle");
    if let Some(kv) = &store.known_version {
        let newer = semver::Version::parse(kv)
            .map(|v| v > running_semver())
            .unwrap_or(false);
        if newer && Some(kv.as_str()) != store.skipped_version.as_deref() {
            s.state = "waiting".into();
            s.version = kv.clone();
        }
    }
    if s.state == "idle" && store.skipped_version.is_some() {
        s.state = "skipped".into();
        s.version = store.skipped_version.clone().unwrap_or_default();
    }
    s
}

// ---------------------------------------------------------------- download

pub fn cancel_download() {
    let mut st = dl_state().lock().unwrap();
    if st.running {
        st.cancel = true;
    }
}

/// Download the best available update: per-seeder manifest verify, Range
/// resume, sha256 gate, atomic rename, viral reseed. `progress(done,total)`.
pub async fn download_app_update(
    http: &reqwest::Client,
    dir: &Path,
    hs: &sync::SharedHost,
    progress: impl Fn(u64, u64),
) -> Result<String> {
    download_app_update_pk(http, dir, hs, progress, UPDATE_PUBKEY).await
}

/// Same as `download_app_update` but with an injectable key (tests).
pub async fn download_app_update_pk(
    http: &reqwest::Client,
    dir: &Path,
    hs: &sync::SharedHost,
    progress: impl Fn(u64, u64),
    pubkey: &str,
) -> Result<String> {
    {
        let mut st = dl_state().lock().unwrap();
        if st.running {
            anyhow::bail!("download already in progress");
        }
        st.running = true;
        st.cancel = false;
        st.done = 0;
        st.total = 0;
    }
    let r = download_inner(http, dir, hs, &progress, pubkey).await;
    let mut st = dl_state().lock().unwrap();
    st.running = false;
    r
}

async fn download_inner(
    http: &reqwest::Client,
    dir: &Path,
    hs: &sync::SharedHost,
    progress: &impl Fn(u64, u64),
    pubkey: &str,
) -> Result<String> {
    let store = load_store(dir);
    let skipped = store.skipped_version.clone();
    let candidates: Vec<FoundSeeder> = found_seeders()
        .into_iter()
        .filter(|f| {
            semver::Version::parse(&f.bundle_version)
                .map(|v| v > running_semver())
                .unwrap_or(false)
        })
        .take(MAX_SEEDERS)
        .collect();
    if candidates.is_empty() {
        anyhow::bail!("no seeder available");
    }

    let mut last_err: Option<anyhow::Error> = None;
    for seeder in &candidates {
        match try_download_from(http, dir, seeder, skipped.as_deref(), progress, pubkey).await {
            Ok(msg) => {
                // Viral reseed: verified bundle becomes ours to serve.
                let _ = reseed(dir);
                let _ = sync::ensure_server(hs);
                // Remember a newer version exists even if seeders vanish.
                let mut s = load_store(dir);
                s.known_version = Some(seeder.bundle_version.clone());
                save_store(dir, &s);
                return Ok(msg);
            }
            Err(e) => {
                if dl_state().lock().unwrap().cancel {
                    anyhow::bail!("cancelled");
                }
                found_map().lock().unwrap().remove(&seeder.addr);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("all seeders failed")))
}

async fn try_download_from(
    http: &reqwest::Client,
    dir: &Path,
    seeder: &FoundSeeder,
    skipped: Option<&str>,
    progress: &impl Fn(u64, u64),
    pubkey: &str,
) -> Result<String> {
    let (m, man_bytes, sig_text) = verified_manifest(http, &seeder.addr, skipped, pubkey).await?;
    let dl = dl_dir(dir);
    std::fs::create_dir_all(&dl)?;
    let final_path = dl.join(&m.file);
    let part_path = dl.join(format!("{}.part", m.file));

    // Free-space preflight.
    let free = fs2::free_space(&dl).unwrap_or(u64::MAX);
    if m.size > 0 && free < m.size {
        anyhow::bail!("not enough disk space");
    }

    // Resume from a .part if present.
    let mut have = part_path.metadata().map(|x| x.len()).unwrap_or(0);
    if have > m.size {
        let _ = std::fs::remove_file(&part_path);
        have = 0;
    }
    let mut req = http
        .get(format!("http://{}/app/file?name={}", seeder.addr, url_encode(&m.file)))
        .timeout(Duration::from_secs(15));
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let mut r = req.send().await?;
    let status = r.status().as_u16();
    if status == 416 {
        let _ = std::fs::remove_file(&part_path);
        anyhow::bail!("range rejected");
    }
    r.error_for_status_ref()?;
    let append = status == 206 && have > 0;
    if let Some(len) = r.content_length() {
        if !append && len != m.size {
            anyhow::bail!("size mismatch from seeder");
        }
    }
    use std::io::Write;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part_path)?;
    let mut done = if append { have } else { 0 };
    {
        let mut st = dl_state().lock().unwrap();
        st.done = done;
        st.total = m.size;
    }
    while let Some(chunk) = r.chunk().await? {
        if dl_state().lock().unwrap().cancel {
            anyhow::bail!("cancelled");
        }
        out.write_all(&chunk)?;
        done += chunk.len() as u64;
        dl_state().lock().unwrap().done = done;
        progress(done, m.size);
    }
    drop(out);
    if done != m.size {
        anyhow::bail!("incomplete download");
    }
    // Verify payload against the signed manifest.
    let h = sha256_file(&part_path)?;
    if h != m.sha256 {
        let _ = std::fs::remove_file(&part_path);
        anyhow::bail!("hash mismatch — payload rejected");
    }
    std::fs::rename(&part_path, &final_path)?;
    // Stash manifest+sig so reseed can rebuild the bundle offline.
    save_bundle_meta(dir, &man_bytes, &sig_text, &m.file);
    // Persist the ready-to-install entry.
    let mut s = load_store(dir);
    s.downloaded = Some(DownloadedEntry {
        version: m.version.clone(),
        path: final_path.to_string_lossy().into_owned(),
        sha256: m.sha256.clone(),
        notes: m.notes.clone(),
    });
    s.known_version = Some(m.version.clone());
    save_store(dir, &s);
    Ok(format!("✓ downloaded v{} — ready to install", m.version))
}

/// Copy the verified bundle into our own seed dir (copy-then-activate).
fn reseed(dir: &Path) -> Result<()> {
    let s = load_store(dir);
    let Some(d) = &s.downloaded else { return Ok(()) };
    let seed = seed_dir(dir);
    std::fs::create_dir_all(&seed)?;
    let payload = PathBuf::from(&d.path);
    if !payload.is_file() {
        return Ok(());
    }
    // Manifest + sig were stashed at download time so reseed doesn't depend
    // on contacting the seeder again.
    let Some((man_bytes, sig, file)) = saved_manifest(dir) else {
        return Ok(());
    };
    for (name, bytes) in [
        ("latest.json", man_bytes),
        ("latest.json.minisig", sig.into_bytes()),
    ] {
        let tmp = seed.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, seed.join(name))?;
    }
    let tmp = seed.join(format!("{file}.tmp"));
    std::fs::copy(&payload, &tmp)?;
    std::fs::rename(&tmp, seed.join(&file))?;
    Ok(())
}

/// Stash the verified manifest+sig in the store at download time so reseed
/// can reconstruct the bundle without contacting the seeder again.
fn save_bundle_meta(dir: &Path, manifest_bytes: &[u8], sig: &str, file: &str) {
    let meta = dir.join("updates-dl/.bundle-meta");
    let j = serde_json::json!({
        "manifest": String::from_utf8_lossy(manifest_bytes),
        "sig": sig,
        "file": file,
    });
    let _ = std::fs::write(&meta, j.to_string());
}

fn saved_manifest(dir: &Path) -> Option<(Vec<u8>, String, String)> {
    let meta = dir.join("updates-dl/.bundle-meta");
    let j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta).ok()?).ok()?;
    Some((
        j["manifest"].as_str()?.as_bytes().to_vec(),
        j["sig"].as_str()?.to_string(),
        j["file"].as_str()?.to_string(),
    ))
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------- install

/// Re-verify the staged download and back up the current exe (escape hatch).
/// Returns the installer path — the caller decides when to spawn+exit.
pub fn prepare_install(dir: &Path) -> Result<PathBuf> {
    let s = load_store(dir);
    let d = s.downloaded.context("nothing downloaded")?;
    let p = PathBuf::from(&d.path);
    if !p.is_file() {
        anyhow::bail!("downloaded installer missing");
    }
    if sha256_file(&p)? != d.sha256 {
        anyhow::bail!("downloaded installer failed hash check");
    }
    // Escape hatch: keep the running exe so a broken release can't strand us.
    if let Ok(exe) = std::env::current_exe() {
        let prev = dl_dir(dir).join("prev");
        let _ = std::fs::create_dir_all(&prev);
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ModpackManager-prev.exe".into());
        let _ = std::fs::copy(&exe, prev.join(name));
    }
    Ok(p)
}

/// Prepare + spawn the installer detached. The caller exits the app after.
/// The installer runs SILENT (/S — no dialogs), then relaunches the updated
/// app once it finishes, so an update is a single click end-to-end.
pub fn install_app_update(dir: &Path) -> Result<String> {
    let p = prepare_install(dir)?;
    // Run via a .cmd so we can wait for the install to finish before relaunching
    // the app. The 2s delay gives our process time to exit before NSIS starts —
    // a running app would block the file swap or trigger a close prompt.
    let script = dl_dir(dir).join("update.cmd");
    let exe = std::env::current_exe()
        .map(|e| e.display().to_string())
        .unwrap_or_default();
    let body = format!(
        "@echo off\r\n\"%SystemRoot%\\System32\\timeout.exe\" /t 2 /nobreak >nul\r\n\"{}\" /S\r\nstart \"\" \"{}\"\r\n",
        p.display(),
        exe
    );
    std::fs::write(&script, body).context("failed to write update.cmd")?;
    let mut cmd = std::process::Command::new("cmd.exe");
    cmd.args(["/c", &script.display().to_string()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW — no console flash
    }
    cmd.spawn().context("failed to launch installer")?;
    Ok(format!("installing {}", p.display()))
}

// ---------------------------------------------------------------- seed pick

/// Validate a folder containing latest.json + .minisig + payload, then seed it
/// (copy-then-activate). Strictly re-verifies before anything goes live.
pub fn pick_update_bundle(hs: &sync::SharedHost, dir: &Path, folder: &str) -> Result<String> {
    pick_update_bundle_pk(hs, dir, folder, UPDATE_PUBKEY)
}

/// Same as `pick_update_bundle` but with an injectable key (tests).
pub fn pick_update_bundle_pk(
    hs: &sync::SharedHost,
    dir: &Path,
    folder: &str,
    pubkey: &str,
) -> Result<String> {
    let src = PathBuf::from(folder);
    if !src.is_dir() {
        anyhow::bail!("not a directory: {folder}");
    }
    let man_bytes = std::fs::read(src.join("latest.json")).context("no latest.json")?;
    let sig_bytes = std::fs::read(src.join("latest.json.minisig")).context("no .minisig")?;
    let sig_text = String::from_utf8_lossy(&sig_bytes).into_owned();
    verify_manifest_sig(&man_bytes, &sig_text, pubkey)?;
    let m: UpdateManifest = serde_json::from_slice(&man_bytes).context("bad manifest")?;
    if !bare_filename_ok(&m.file) {
        anyhow::bail!("bad filename in manifest");
    }
    let payload = src.join(&m.file);
    if !payload.is_file() {
        anyhow::bail!("payload missing: {}", m.file);
    }
    if sha256_file(&payload)? != m.sha256 {
        anyhow::bail!("payload sha256 mismatch — bundle rejected");
    }
    let seed = seed_dir(dir);
    std::fs::create_dir_all(&seed)?;
    for (name, bytes) in [
        ("latest.json", man_bytes.clone()),
        ("latest.json.minisig", sig_bytes),
    ] {
        let tmp = seed.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, seed.join(name))?;
    }
    let tmp = seed.join(format!("{}.tmp", m.file));
    std::fs::copy(&payload, &tmp)?;
    std::fs::rename(&tmp, seed.join(&m.file))?;
    let _ = sync::ensure_server(hs);
    Ok(format!("✓ seeding update v{}", m.version))
}

// ---------------------------------------------------------------- launch purge

/// Dismiss the post-update "✓ updated" banner permanently.
pub fn ack_update_banner(dir: &Path) {
    let mut s = load_store(dir);
    if s.welcome_pending.is_some() {
        s.welcome_pending = None;
        save_store(dir, &s);
    }
}

/// Run once at startup: drop installed/superseded downloads, stale parts,
/// orphan .tmp files, and seed bundles older than what we're running.
/// Seed purges at strictly-older only — a just-updated instance keeps seeding
/// the very release it now runs.
pub fn launch_purge(dir: &Path) {
    let running = running_semver();
    let mut s = load_store(dir);
    let mut dirty = false;
    if let Some(d) = &s.downloaded {
        let stale = semver::Version::parse(&d.version)
            .map(|v| v <= running)
            .unwrap_or(true);
        if stale {
            // A staged installer equal to what now runs = it was just
            // installed via the updater — keep its notes and owe the
            // "✓ updated" banner even without a prior last_run record.
            if semver::Version::parse(&d.version).map(|v| v == running).unwrap_or(false) {
                if !d.notes.is_empty() {
                    s.installed_notes = d.notes.clone();
                }
                if s.welcome_pending.is_none() {
                    s.welcome_pending = Some(d.version.clone());
                }
            }
            let _ = std::fs::remove_file(&d.path);
            s.downloaded = None;
            dirty = true;
        }
    }
    if let Some(kv) = &s.known_version {
        if semver::Version::parse(kv).map(|v| v <= running).unwrap_or(true) {
            s.known_version = None;
            dirty = true;
        }
    }
    if dirty {
        save_store(dir, &s);
    }
    // Stale .part / .tmp (>24h) sweeps.
    let cutoff = SystemTime::now() - Duration::from_secs(24 * 3600);
    for dirp in [dl_dir(dir), seed_dir(dir)] {
        if let Ok(rd) = std::fs::read_dir(&dirp) {
            for e in rd.flatten() {
                let p = e.path();
                let is_tmp = p
                    .extension()
                    .map(|x| x == "part" || x == "tmp")
                    .unwrap_or(false)
                    || p.file_name()
                        .map(|n| n.to_string_lossy().ends_with(".part") || n.to_string_lossy().ends_with(".tmp"))
                        .unwrap_or(false);
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t < cutoff)
                    .unwrap_or(false);
                if is_tmp && old {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
    // Superseded seed → stop advertising old builds.
    if let Some((m, _)) = seed_manifest(dir) {
        if semver::Version::parse(&m.version)
            .map(|v| v < running)
            .unwrap_or(true)
        {
            let seed = seed_dir(dir);
            if let Ok(rd) = std::fs::read_dir(&seed) {
                for e in rd.flatten() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}
