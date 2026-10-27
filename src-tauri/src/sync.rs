//! LAN sync networks — a host shares a mods folder over HTTP; clients
//! auto-discover it (UDP broadcast), join with a code, and pull delta
//! updates. Built for Radmin-style virtual LANs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{launchers, pack, scanner};
use launchers::LauncherInfo;

pub const HTTP_PORT: u16 = 47676;
pub const BROADCAST_PORT: u16 = 47677;
const CODE_HEADER: &str = "X-Join-Code";
const MEMBER_HEADER: &str = "X-Member-Id";
const HEARTBEAT_SECS: u64 = 15;
const ONLINE_WINDOW_SECS: u64 = 135;
/// Missed host contacts before `host_online` flips off — one dropped
/// heartbeat on a congested link must not flap the badge.
const OFFLINE_MISSES: u32 = 3;
/// How long a found network stays listed after its announces stop.
const FOUND_TTL_SECS: u64 = 45;
/// A *stale* (remembered, not announcing) found entry survives this long.
const STALE_TTL_SECS: u64 = 14 * 24 * 3600;
/// Per-file GET ceiling — generous because per-chunk stall timeouts and
/// retry/resume handle real stalls; this only bounds a dead connection.
const FILE_TIMEOUT_SECS: u64 = 3600;
/// Per-chunk stall guard — no bytes for this long → attempt fails, retry.
const CHUNK_TIMEOUT_SECS: u64 = 60;
const FILE_RETRIES: usize = 3;
const PARALLEL_FILES: usize = 4;
/// Progress event throttle — one emit per interval max, always emit
/// file-start/file-done/final so the UI never misses a boundary.
const EMIT_MS: u128 = 120;

/// Set by the tray layer — while the window is hidden, background loops
/// slow down (never stop) so hosting/presence survive efficiency mode.
pub static APP_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Wakes member heartbeat loops early — a returning user shouldn't wait
/// out the 60s hidden-mode sleep. `watch` (not Notify) because several
/// loops share the signal: receivers always observe the latest generation,
/// even loops that were mid-request when it fired.
static HB_WAKE_GEN: OnceLock<tokio::sync::watch::Sender<u64>> = OnceLock::new();

fn hb_wake_gen() -> &'static tokio::sync::watch::Sender<u64> {
    HB_WAKE_GEN.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Set by the cancel command — the apply engine checks it between chunks.
static APPLY_CANCEL: AtomicBool = AtomicBool::new(false);

pub fn set_hidden(hidden: bool) {
    if APP_HIDDEN.swap(hidden, Ordering::Relaxed) != hidden {
        crate::alog!(
            "tray",
            "window {} — loops {}",
            if hidden { "hidden" } else { "shown" },
            if hidden { "slowed" } else { "normal" }
        );
        if !hidden {
            // Bump the generation — every heartbeat loop observes it on
            // its next wait (or gets its current wait cut short).
            hb_wake_gen().send_modify(|g| *g += 1);
            // Burst: members refresh presence/discovery and a host announces
            // immediately instead of waiting out the slowed hidden tick.
            std::thread::spawn(|| {
                query_networks();
                announce_now();
            });
        }
    }
}

pub fn cancel_apply() {
    APPLY_CANCEL.store(true, Ordering::Relaxed);
    crate::alog!("xfer", "cancel requested");
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn new_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:016x}", n ^ (std::process::id() as u128))
}

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String, // relative to shared folder, '/'-separated
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "networkId")]
    pub network_id: String,
    #[serde(rename = "packName")]
    pub pack_name: String,
    /// Human-readable pack version the host picked ("1.3", "beta 2") —
    /// display only; `version` is still the monotonic sync counter.
    #[serde(rename = "packVersion", default)]
    pub pack_version: String,
    pub version: u64,
    pub note: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: u64,
    pub name: String,
    pub last_seen: u64,
    pub applied_version: u64,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoundNetwork {
    #[serde(rename = "networkId")]
    pub network_id: String,
    pub name: String,
    #[serde(rename = "packName")]
    pub pack_name: String,
    pub machine: String,
    pub addr: String,
    #[serde(rename = "latestVersion")]
    pub latest_version: u64,
    pub members: usize,
    /// The host's app version (display only).
    #[serde(rename = "appVersion", default)]
    pub app_version: String,
    /// Whether the network requires a join code. `None` = the announce
    /// didn't carry the field (host older than 0.1.9) → treat as unknown.
    #[serde(rename = "requiresCode", skip_serializing_if = "Option::is_none", default)]
    pub requires_code: Option<bool>,
    /// When the last announce for this network arrived. Live entries silent
    /// past TTL are dropped; stale ones persist for "last seen" display.
    #[serde(rename = "lastSeen", default)]
    pub last_seen: u64,
    /// `true` while announces keep refreshing this entry; `false` = a
    /// remembered entry from the store — shown dimmed until re-confirmed.
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberView {
    // String, not u64: ids exceed 2^53, and JS serializes large JSON
    // numbers through f64 — a numeric id round-trips back as a DIFFERENT
    // integer and kick/targeted-publish silently hit a non-existent key.
    pub id: String,
    pub name: String,
    pub online: bool,
    #[serde(rename = "appliedVersion")]
    pub applied_version: u64,
    #[serde(rename = "upToDate")]
    pub up_to_date: bool,
    /// Has a targeted (member-only) update waiting.
    pub targeted: bool,
    pub host: bool,
    /// The member's app version (display only).
    #[serde(rename = "appVersion")]
    pub app_version: String,
}

#[derive(Debug, Serialize)]
pub struct PendingUpdate {
    pub version: u64,
    #[serde(rename = "packName")]
    pub pack_name: String,
    pub note: String,
    #[serde(rename = "fileCount")]
    pub file_count: usize,
    #[serde(rename = "toDownload")]
    pub to_download: usize,
    #[serde(rename = "toDelete")]
    pub to_delete: usize,
}

#[derive(Debug, Serialize)]
pub struct ConnectStatus {
    pub mode: String, // "idle" | "hosting" | "joined"
    // host fields
    #[serde(rename = "networkName")]
    pub network_name: Option<String>,
    #[serde(rename = "networkId")]
    pub network_id: Option<String>,
    pub code: Option<String>,
    pub folder: Option<String>,
    /// Pack meta shown/edited at publish time (host only).
    #[serde(rename = "packName")]
    pub pack_name: Option<String>,
    #[serde(rename = "packVersion")]
    pub pack_version: Option<String>,
    #[serde(rename = "mcVersion")]
    pub mc_version: Option<String>,
    pub loader: Option<String>,
    #[serde(rename = "loaderVersion")]
    pub loader_version: Option<String>,
    /// Folder the pack meta was last confirmed for — a mismatch means the
    /// folder moved underneath the meta → re-suggest instead of prefilling.
    #[serde(rename = "metaFolder")]
    pub meta_folder: Option<String>,
    #[serde(rename = "latestVersion")]
    pub latest_version: u64,
    pub history: Vec<serde_json::Value>,
    // client fields
    #[serde(rename = "hostAddr")]
    pub host_addr: Option<String>,
    #[serde(rename = "hostOnline")]
    pub host_online: bool,
    #[serde(rename = "appliedVersion")]
    pub applied_version: u64,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    // shared
    pub members: Vec<MemberView>,
    pub pending: Option<PendingUpdate>,
    /// True once, on the status call that learns we were kicked — the client
    /// session is then cleared.
    pub kicked: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncedPackInfo {
    #[serde(rename = "networkId")]
    pub network_id: String,
    pub name: String,
    pub version: u64,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
    #[serde(rename = "fileCount")]
    pub file_count: usize,
}

pub struct HostState {
    pub network_id: String,
    pub name: String,
    pub code: String,
    pub folder: PathBuf,
    pub pack_name: String,
    pub pack_version: String,
    pub mc_version: String,
    pub loader: String,
    pub loader_version: String,
    /// Folder the pack meta was last confirmed/edited for — the publish
    /// dialog re-suggests when this doesn't match `folder`.
    pub meta_folder: Option<String>,
    pub manifest: Manifest,
    /// member_id -> a manifest served only to that member (targeted publish).
    /// Versions share `counter` with the global stream so they never collide.
    pub targets: HashMap<u64, Manifest>,
    pub counter: u64,
    pub history: Vec<(u64, String, u64)>, // (version, note, ts)
    pub members: HashMap<u64, Member>,
    /// Kicked member ids — blocked from heartbeat until they rejoin.
    pub banned: HashSet<u64>,
    /// Where connect.json lives — HTTP handlers persist roster changes
    /// through it without needing the path threaded in.
    pub store_path: PathBuf,
    pub stop: Arc<AtomicBool>,
}

pub struct ClientState {
    pub network_id: String,
    pub network_name: String,
    pub host_addr: String,
    pub code: String,
    pub member_id: u64,
    pub display_name: String,
    pub applied_version: u64,
    pub declined: Vec<u64>,
    pub synced_dir: PathBuf,
    pub host_online: bool,
    /// Consecutive failed host contacts — `host_online` only drops after
    /// OFFLINE_MISSES in a row, so one congested beat can't flap the badge.
    pub consec_failures: u32,
    /// True while apply_update_progress is transferring — connect_status
    /// skips its manifest poll so polls don't compete with the download.
    pub applying: bool,
    /// Host kicked us — shown once, then the session is cleared.
    pub kicked: bool,
    pub member_views: Vec<MemberView>,
    pub latest_version: u64,
    pub latest_note: String,
    /// rel path → (size, mtime secs, sha1) — diff_manifest skips re-hashing
    /// files whose size+mtime haven't moved. In-memory only.
    pub hash_memo: HashMap<String, (u64, u64, String)>,
    /// Per-join token — the heartbeat task exits when this no longer
    /// matches, so a rejoin can't leave two heartbeat loops running.
    pub hb_token: u64,
}

pub type SharedHost = Arc<Mutex<Option<HostState>>>;
pub type SharedClient = Arc<Mutex<Option<ClientState>>>;

// ------------------------------------------------------- persistence

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub host: Option<SavedHost>,
    pub client: Option<SavedClient>,
    /// Remembered discovered networks — seeded on start so the found list
    /// isn't empty while queries are in flight. They come back stale.
    #[serde(default)]
    pub found: Vec<FoundNetwork>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedHost {
    pub name: String,
    pub code: String,
    pub folder: String,
    #[serde(rename = "networkId")]
    pub network_id: String,
    #[serde(rename = "packName")]
    pub pack_name: String,
    #[serde(rename = "packVersion", default)]
    pub pack_version: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
    #[serde(rename = "metaFolder", default)]
    pub meta_folder: Option<String>,
    /// Last explicitly published manifest — restored verbatim on restart.
    /// Publishing is NEVER automatic, so this is always a human action.
    #[serde(default)]
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub counter: u64,
    #[serde(default)]
    pub history: Vec<(u64, String, u64)>,
    #[serde(default)]
    pub banned: Vec<u64>,
    /// The member roster — persisted so a host restart doesn't wipe the
    /// table; offline members reappear as offline instead of vanishing.
    #[serde(default)]
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedClient {
    #[serde(rename = "networkId")]
    pub network_id: String,
    #[serde(rename = "networkName")]
    pub network_name: String,
    #[serde(rename = "hostAddr")]
    pub host_addr: String,
    pub code: String,
    #[serde(rename = "memberId")]
    pub member_id: u64,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "appliedVersion")]
    pub applied_version: u64,
    pub declined: Vec<u64>,
}

pub fn store_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("modpack-manager")
        .join("connect.json")
}

/// The discovery-listener thread saves found entries on its own — it needs
/// a store path without a parameter. Sandboxes override it to a temp dir.
static STORE_PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_store_path_override(p: PathBuf) {
    let _ = STORE_PATH_OVERRIDE.set(p);
}

fn effective_store_path() -> PathBuf {
    STORE_PATH_OVERRIDE.get().cloned().unwrap_or_else(store_path)
}

fn load_store(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // Atomic: a reader (or a crash) must never see a half-written store.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(store).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Every load+modify+save MUST hold this — a writer working off a stale
/// snapshot clobbers concurrent updates (the discovery listener's
/// `save_found` once dropped a roster row written mid-flight).
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn update_store(path: &Path, f: impl FnOnce(&mut Store)) {
    let _g = STORE_LOCK.lock().unwrap();
    let mut store = load_store(path);
    f(&mut store);
    save_store(path, &store);
}

fn save_host(store_path: &Path, h: &HostState) {
    update_store(store_path, |store| {
        store.host = Some(SavedHost {
            name: h.name.clone(),
            code: h.code.clone(),
            folder: h.folder.to_string_lossy().into_owned(),
            network_id: h.network_id.clone(),
            pack_name: h.pack_name.clone(),
            pack_version: h.pack_version.clone(),
            mc_version: h.mc_version.clone(),
            loader: h.loader.clone(),
            loader_version: h.loader_version.clone(),
            meta_folder: h.meta_folder.clone(),
            manifest: Some(h.manifest.clone()),
            counter: h.counter,
            history: h.history.clone(),
            banned: h.banned.iter().copied().collect(),
            members: h.members.values().cloned().collect(),
        });
    });
}

fn save_client(store_path: &Path, c: &ClientState) {
    update_store(store_path, |store| {
        store.client = Some(SavedClient {
            network_id: c.network_id.clone(),
            network_name: c.network_name.clone(),
            host_addr: c.host_addr.clone(),
            code: c.code.clone(),
            member_id: c.member_id,
            display_name: c.display_name.clone(),
            applied_version: c.applied_version,
            declined: c.declined.clone(),
        });
    });
}

fn clear_saved_host(store_path: &Path) {
    update_store(store_path, |store| store.host = None);
}

fn clear_saved_client(store_path: &Path) {
    update_store(store_path, |store| store.client = None);
}

// ------------------------------------------------------- manifest

/// Hash all jars under `folder` into a manifest file list (relative paths).
fn manifest_files_from_scan(
    res: &scanner::ScanResult,
    folder: &Path,
) -> Result<Vec<ManifestFile>> {
    let mut files: Vec<ManifestFile> = Vec::with_capacity(res.mods.len());
    for m in &res.mods {
        let Ok(rel) = Path::new(&m.path).strip_prefix(folder) else { continue };
        // Scanner leaves sha1 empty only when the file couldn't be opened —
        // hash it here so the error surfaces at publish instead of shipping
        // a manifest members can never verify.
        let sha1 = if m.sha1.is_empty() { sha1_file(Path::new(&m.path))? } else { m.sha1.clone() };
        files.push(ManifestFile {
            path: rel.to_string_lossy().replace('\\', "/"),
            sha1,
            size: m.size,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn build_file_list(folder: &Path) -> Result<Vec<ManifestFile>> {
    let res = scanner::scan_folder_progress(&folder.to_string_lossy(), |_, _| {})?;
    manifest_files_from_scan(&res, folder)
}

/// Guess (mc_version, loader) from the jars' own metadata — majority vote
/// over normalized loader names and the newest most-common MC version.
/// A shared mods folder isn't a built pack, so this is how pack meta gets
/// filled when the host doesn't type it in.
/// Extract a concrete MC version from a jar's declared range/exact string:
/// "[1.21.1]" → "1.21.1", "[1.21.1,)" → "1.21.1", "(,1.22]" → "1.22",
/// "1.21.1" → "1.21.1". Ranges take the lower bound — the vote then
/// decides the majority MC across the whole folder.
fn mc_from_range(v: &str) -> String {
    let t = v.trim().trim_matches(|c| "[]()".contains(c)).to_string();
    let mut parts = t.split(',').map(|p| p.trim().trim_matches(|c| "[]()".contains(c)));
    let lower = parts.next().unwrap_or("");
    if !lower.is_empty() {
        return lower.to_string();
    }
    parts.next().map(|s| s.to_string()).unwrap_or_else(|| t.clone())
}

pub fn suggest_meta(mods: &[scanner::ModInfo]) -> (String, String) {
    let normalize = |l: &str| match l {
        "fabric-loader" | "fabricmc" => "fabric".to_string(),
        "quilt-loader" => "quilt".to_string(),
        other => other.to_string(),
    };
    let mut loader_votes: HashMap<String, usize> = HashMap::new();
    let mut ver_votes: HashMap<String, usize> = HashMap::new();
    for m in mods {
        for l in &m.loaders {
            *loader_votes.entry(normalize(l)).or_default() += 1;
        }
        for v in &m.game_versions {
            let v = mc_from_range(v);
            if !v.is_empty() {
                *ver_votes.entry(v).or_default() += 1;
            }
        }
    }
    let loader = loader_votes
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(l, _)| l)
        .unwrap_or_default();
    let mc = ver_votes
        .into_iter()
        // most jars agreeing wins; ties go to the higher version string
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)))
        .map(|(v, _)| v)
        .unwrap_or_default();
    (mc, loader)
}

fn sha1_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut h = sha1::Sha1::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Reject anything that could escape the base dir.
fn safe_rel(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.replace('\\', "/");
    if rel.contains("..") || rel.starts_with('/') || rel.contains(':') {
        return None;
    }
    Some(base.join(rel))
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

// ------------------------------------------------------- host

#[allow(clippy::too_many_arguments)]
pub fn host_start(
    hs: &SharedHost,
    store_path: &Path,
    name: &str,
    code: &str,
    folder: &str,
    pack_name: &str,
    mc_version: &str,
    loader: &str,
    loader_version: &str,
) -> Result<String> {
    set_query_host(hs);
    host_stop(hs, store_path);
    let folder_path = PathBuf::from(folder);
    if !folder_path.is_dir() {
        anyhow::bail!("not a directory: {folder}");
    }
    // One scan covers both jobs: the staged file list AND meta auto-fill —
    // jars declare their loader + game version, so a plain mods folder
    // doesn't need Build-tab settings to be installable downstream.
    let res = scanner::scan_folder_progress(folder, |_, _| {})?;
    let n_files = manifest_files_from_scan(&res, &folder_path)?.len();
    let network_id = new_id();
    let (sug_mc, sug_loader) = suggest_meta(&res.mods);
    let mc_version = if mc_version.trim().is_empty() { sug_mc } else { mc_version.into() };
    let loader = if loader.trim().is_empty() {
        if sug_loader.is_empty() { "fabric".into() } else { sug_loader }
    } else {
        loader.into()
    };
    // Pack name defaults to the shared folder's basename — friendlier than a
    // blank pack name in announcements before the first publish.
    let pack_name = if pack_name.trim().is_empty() {
        folder_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "My Modpack".into())
    } else {
        pack_name.to_string()
    };
    // STRICT: nothing is published until the Publish button is pressed.
    // The manifest starts at v0 / empty — clients see "nothing published".
    let manifest = Manifest {
        network_id: network_id.clone(),
        pack_name: pack_name.clone(),
        pack_version: String::new(),
        version: 0,
        note: String::new(),
        mc_version: mc_version.clone(),
        loader: loader.clone(),
        loader_version: loader_version.into(),
        files: vec![],
    };
    let stop = Arc::new(AtomicBool::new(false));
    let st = HostState {
        network_id,
        name: name.to_string(),
        code: code.to_string(),
        folder: folder_path.clone(),
        pack_name,
        pack_version: String::new(),
        mc_version,
        loader,
        loader_version: loader_version.into(),
        // Auto-detected at start — counts as "for this folder" so the first
        // publish dialog prefills instead of rescanning.
        meta_folder: Some(folder.to_string()),
        history: vec![],
        manifest,
        targets: HashMap::new(),
        counter: 0,
        members: HashMap::new(),
        banned: HashSet::new(),
        store_path: store_path.to_path_buf(),
        stop: stop.clone(),
    };
    *hs.lock().unwrap() = Some(st);

    if !ensure_server(hs) {
        *hs.lock().unwrap() = None;
        anyhow::bail!("can't bind port {HTTP_PORT} — another instance is using it");
    }
    let ann_state = hs.clone();
    let ann_stop = stop.clone();
    std::thread::spawn(move || announce_loop(ann_state, ann_stop));

    if let Some(h) = hs.lock().unwrap().as_ref() {
        save_host(store_path, h);
    }
    crate::alog!(
        "host",
        "hosting \"{name}\" — {n_files} file(s) staged{code_str}",
        code_str = if code.is_empty() { ", open network" } else { ", join code set" }
    );
    Ok(format!(
        "✓ hosting \"{name}\" — {n_files} file(s) staged, nothing published yet (code: {code})"
    ))
}

/// Re-host after an app restart: restores the last saved host verbatim —
/// same network id, same manifest, same version counter. Never re-hashes or
/// republishes the folder; members see exactly what was published before.
fn host_resume(hs: &SharedHost, store_path: &Path, saved: &SavedHost) -> Result<()> {
    if hs.lock().unwrap().is_some() {
        return Ok(());
    }
    let manifest = saved.manifest.clone().unwrap_or_else(|| Manifest {
        network_id: saved.network_id.clone(),
        pack_name: saved.pack_name.clone(),
        pack_version: saved.pack_version.clone(),
        version: 0,
        note: String::new(),
        mc_version: saved.mc_version.clone(),
        loader: saved.loader.clone(),
        loader_version: saved.loader_version.clone(),
        files: vec![],
    });
    let stop = Arc::new(AtomicBool::new(false));
    *hs.lock().unwrap() = Some(HostState {
        network_id: saved.network_id.clone(),
        name: saved.name.clone(),
        code: saved.code.clone(),
        folder: PathBuf::from(&saved.folder),
        pack_name: saved.pack_name.clone(),
        pack_version: saved.pack_version.clone(),
        mc_version: saved.mc_version.clone(),
        loader: saved.loader.clone(),
        loader_version: saved.loader_version.clone(),
        meta_folder: saved.meta_folder.clone().filter(|s| !s.is_empty()),
        manifest,
        targets: HashMap::new(),
        counter: saved.counter,
        history: saved.history.clone(),
        // Restore the roster — offline members come back as offline rows
        // instead of vanishing until they heartbeat again.
        members: saved
            .members
            .iter()
            .map(|m| (m.id, m.clone()))
            .collect(),
        banned: saved.banned.iter().copied().collect(),
        store_path: store_path.to_path_buf(),
        stop: stop.clone(),
    });
    // Retry-bind happens inside announce_loop if the port is busy right now.
    let _ = ensure_server(hs);
    let ann_state = hs.clone();
    let ann_stop = stop.clone();
    std::thread::spawn(move || announce_loop(ann_state, ann_stop));
    crate::alog!(
        "host",
        "resumed \"{}\" — {} member(s) restored",
        saved.name,
        saved.members.len()
    );
    Ok(())
}

/// Host-only: rename the network and/or toggle/change the join code.
/// Empty `code` = open network. Existing members are unaffected — they
/// authenticate by member id, so a code change only gates new joins.
#[allow(clippy::too_many_arguments)]
pub fn host_update(
    hs: &SharedHost,
    store_path: &Path,
    name: &str,
    code: &str,
    pack_name: &str,
    pack_version: &str,
    mc_version: &str,
    loader: &str,
    loader_version: &str,
) -> Result<String> {
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    h.name = if name.trim().is_empty() { "My Network".into() } else { name.trim().into() };
    h.code = code.to_string();
    // Pack meta: empty means "leave unchanged". Updating the live manifest
    // (and any targeted manifests) makes the fix visible to members on
    // their next check — no republish needed just to correct metadata.
    if !pack_name.trim().is_empty() {
        h.pack_name = pack_name.trim().into();
    }
    if !pack_version.trim().is_empty() {
        h.pack_version = pack_version.trim().into();
    }
    if !mc_version.trim().is_empty() {
        h.mc_version = mc_version.trim().into();
    }
    if !loader.trim().is_empty() {
        h.loader = loader.trim().into();
    }
    if !loader_version.trim().is_empty() {
        h.loader_version = loader_version.trim().into();
    }
    h.manifest.pack_name = h.pack_name.clone();
    h.manifest.pack_version = h.pack_version.clone();
    h.manifest.mc_version = h.mc_version.clone();
    h.manifest.loader = h.loader.clone();
    h.manifest.loader_version = h.loader_version.clone();
    for m in h.targets.values_mut() {
        m.pack_name = h.pack_name.clone();
        m.pack_version = h.pack_version.clone();
        m.mc_version = h.mc_version.clone();
        m.loader = h.loader.clone();
        m.loader_version = h.loader_version.clone();
    }
    // Explicitly edited → this meta is confirmed for the current folder.
    h.meta_folder = Some(h.folder.to_string_lossy().into_owned());
    crate::alog!(
        "host",
        "network updated — \"{}\"{} (pack \"{}\" {} {})",
        h.name,
        if h.code.is_empty() { " (open)" } else { " (join code set)" },
        h.pack_name,
        h.mc_version,
        h.loader,
    );
    save_host(store_path, h);
    Ok(format!(
        "✓ network updated — \"{}\"{}",
        h.name,
        if h.code.is_empty() { " (open, no join code)" } else { " (join code set)" }
    ))
}

/// Host-only: remove a member and block their heartbeats until they rejoin
/// with the join code (if one is set).
pub fn host_kick(hs: &SharedHost, store_path: &Path, member_id: u64) -> Result<String> {
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    let name = h
        .members
        .get(&member_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| format!("member {member_id}"));
    h.members.remove(&member_id);
    h.targets.remove(&member_id); // any pending targeted publish dies with them
    h.banned.insert(member_id);
    crate::alog!("host", "kicked {name} ({member_id}) — banned until rejoin");
    save_host(store_path, h);
    Ok(format!("✓ kicked {name} — they can rejoin with the code"))
}

pub fn host_stop(hs: &SharedHost, store_path: &Path) {
    if let Some(h) = hs.lock().unwrap().take() {
        h.stop.store(true, Ordering::Relaxed);
        crate::alog!("host", "hosting stopped");
    }
    clear_saved_host(store_path);
}

/// Publish the current folder. `target` = a member id → only that member sees
/// this update; `None` → everyone. Versions share one counter so targeted
/// numbers can't collide with future global publishes.
/// `allow_empty` — the UI confirms before passing true; publishing an empty
/// folder would delete every mod on every member.
/// `force` — republish even when files are identical; without it an
/// unchanged folder fails with UNCHANGED and the UI asks to confirm.
/// Pack-meta params come from the publish dialog; empty fields keep current.
#[allow(clippy::too_many_arguments)]
pub fn host_publish(
    hs: &SharedHost,
    store_path: &Path,
    note: &str,
    target: Option<u64>,
    allow_empty: bool,
    force: bool,
    pack_name: &str,
    pack_version: &str,
    mc_version: &str,
    loader: &str,
    loader_version: &str,
) -> Result<String> {
    // Hash outside the lock — build_file_list sha1s every file, and holding
    // the mutex across it stalled heartbeats/manifests for the duration.
    let folder = {
        let g = hs.lock().unwrap();
        g.as_ref().context("not hosting")?.folder.clone()
    };
    let files = build_file_list(&folder)?;
    if files.is_empty() && !allow_empty {
        anyhow::bail!("EMPTY_PACK");
    }
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    // The share folder can't move underneath a publish — that would ship the
    // stale listing under the new folder's name.
    if h.folder != folder {
        anyhow::bail!("folder changed during publish — try again");
    }
    // No-change guard: for a targeted publish, only compare against that
    // member's previous target — republishing the global content TO a member
    // is legitimate (e.g. they declined the global update). UNCHANGED is a
    // sentinel the UI confirms past, retrying with force=true.
    let baseline = match target {
        Some(m) => h.targets.get(&m),
        None => Some(&h.manifest),
    };
    if !force {
        if let Some(b) = baseline {
            if files_eq(&files, &b.files) {
                anyhow::bail!("UNCHANGED");
            }
        }
    }
    // Meta confirmed in the publish dialog — empty fields keep current.
    // Updating the live manifests makes it visible on members' next check.
    if !pack_name.trim().is_empty() {
        h.pack_name = pack_name.trim().into();
    }
    if !pack_version.trim().is_empty() {
        h.pack_version = pack_version.trim().into();
    }
    if !mc_version.trim().is_empty() {
        h.mc_version = mc_version.trim().into();
    }
    if !loader.trim().is_empty() {
        h.loader = loader.trim().into();
    }
    if !loader_version.trim().is_empty() {
        h.loader_version = loader_version.trim().into();
    }
    for m in std::iter::once(&mut h.manifest).chain(h.targets.values_mut()) {
        m.pack_name = h.pack_name.clone();
        m.pack_version = h.pack_version.clone();
        m.mc_version = h.mc_version.clone();
        m.loader = h.loader.clone();
        m.loader_version = h.loader_version.clone();
    }
    h.meta_folder = Some(h.folder.to_string_lossy().into_owned());
    h.counter += 1;
    let version = h.counter;
    let mut m = h.manifest.clone();
    m.version = version;
    m.note = note.to_string();
    m.files = files;
    match target {
        Some(mid) => {
            let name = h
                .members
                .get(&mid)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("member {mid}"));
            h.targets.insert(mid, m.clone());
            h.history
                .push((version, format!("→ {name}: {note}"), now_secs()));
            let out = format!(
                "✓ published v{} to {} only — {} file(s){}",
                version,
                name,
                m.files.len(),
                if note.is_empty() { String::new() } else { format!(" — {note}") }
            );
            crate::alog!("host", "published v{version} → {name} only — {} file(s)", m.files.len());
            save_host(store_path, h);
            Ok(out)
        }
        None => {
            h.manifest = m;
            h.history.push((version, note.to_string(), now_secs()));
            let out = format!(
                "✓ published v{} to everyone — {} file(s){}",
                version,
                h.manifest.files.len(),
                if note.is_empty() { String::new() } else { format!(" — {note}") }
            );
            crate::alog!("host", "published v{version} globally — {} file(s)", h.manifest.files.len());
            save_host(store_path, h);
            Ok(out)
        }
    }
}

/// Swap the folder a running host publishes from. Returns how many of the new
/// files match the current manifest (0-100%) + file count, so the UI can warn
/// when the new folder looks like a completely different pack.
pub fn host_set_folder(hs: &SharedHost, store_path: &Path, folder: &str) -> Result<(u32, usize)> {
    let folder_path = PathBuf::from(folder);
    if !folder_path.is_dir() {
        anyhow::bail!("not a directory: {folder}");
    }
    let files = build_file_list(&folder_path)?;
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    // No published baseline → nothing is being "replaced" — report 100% so
    // the UI doesn't warn about a diff that doesn't exist.
    let same = if h.manifest.files.is_empty() {
        100
    } else {
        let old: std::collections::HashMap<&str, &str> = h
            .manifest
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.sha1.as_str()))
            .collect();
        files
            .iter()
            .filter(|f| old.get(f.path.as_str()) == Some(&f.sha1.as_str()))
            .count()
            * 100
            / files.len().max(1)
    };
    h.folder = folder_path;
    let n = files.len();
    crate::alog!("host", "share folder → {folder} ({n} file(s), {same}% same)");
    save_host(store_path, h);
    Ok((same as u32, n))
}

fn files_eq(a: &[ManifestFile], b: &[ManifestFile]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.path == y.path && x.sha1 == y.sha1)
}

fn member_views(h: &HostState) -> Vec<MemberView> {
    let now = now_secs();
    let mut v: Vec<MemberView> = h
        .members
        .values()
        .map(|m| MemberView {
            id: m.id.to_string(),
            name: m.name.clone(),
            online: now.saturating_sub(m.last_seen) < ONLINE_WINDOW_SECS,
            applied_version: m.applied_version,
            up_to_date: m.applied_version >= h.manifest.version
                && !h.targets.contains_key(&m.id),
            targeted: h.targets.contains_key(&m.id),
            host: false,
            app_version: m.app_version.clone(),
        })
        .collect();
    v.insert(
        0,
        MemberView {
            id: "0".into(),
            name: machine_name(),
            online: true,
            applied_version: h.manifest.version,
            up_to_date: true,
            targeted: false,
            host: true,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );
    v
}

// ---- shared http server (lazy-bound, threaded) ----

static SERVER_BOUND: AtomicBool = AtomicBool::new(false);

/// Lazily bind the shared HTTP server the first time something needs to
/// serve — hosting a network or seeding an app-update bundle. Idempotent;
/// callers (announce loops) retry every tick while a need exists.
/// Requests are handled on per-connection threads so one large file stream
/// (e.g. an 80MB installer) can't starve heartbeats or other fetches.
pub fn ensure_server(hs: &SharedHost) -> bool {
    if SERVER_BOUND.load(Ordering::Relaxed) {
        return true;
    }
    let server = match tiny_http::Server::http(("0.0.0.0", HTTP_PORT)) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("app server bind failed: {e}");
            return false;
        }
    };
    SERVER_BOUND.store(true, Ordering::Relaxed);
    let state = hs.clone();
    std::thread::spawn(move || {
        while let Ok(req) = server.recv() {
            let h = state.clone();
            std::thread::spawn(move || handle_request(&h, req));
        }
    });
    true
}

pub fn server_running() -> bool {
    SERVER_BOUND.load(Ordering::Relaxed)
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

fn handle_request(hs: &SharedHost, mut req: tiny_http::Request) {
    let url = req.url().to_string();
    let method = req.method().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let path = path.to_string();
    let query = query.to_string();

    // App-update endpoints — public by design (signature is the trust anchor).
    if path.starts_with("/app/") {
        return crate::appupdate::handle_app_request(req, &path, &query);
    }

    if path == "/info" {
        let g = hs.lock().unwrap();
        let Some(h) = g.as_ref() else {
            return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
        };
        return respond_json(
            req,
            200,
            &serde_json::json!({
                "networkId": h.network_id,
                "name": h.name,
                "packName": h.pack_name,
                "latestVersion": h.manifest.version,
                "mcVersion": h.mc_version,
                "loader": h.loader,
                "requiresCode": !h.code.is_empty(),
                "appVersion": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    // Everything below needs the join code — OR a known, non-kicked member id
    // (so members aren't dropped when the host rotates the code).
    let mid_hdr = member_header(&req);
    let code_ok = {
        let g = hs.lock().unwrap();
        match g.as_ref() {
            None => {
                drop(g);
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            }
            Some(h) => {
                let given = req
                    .headers()
                    .iter()
                    .find(|x| x.field.equiv(CODE_HEADER))
                    .map(|x| x.value.as_str().to_string());
                let member_ok = mid_hdr
                    .map(|id| h.members.contains_key(&id) && !h.banned.contains(&id))
                    .unwrap_or(false);
                // A kicked member's stored code must NOT unlock the other
                // endpoints — banned ids may only reach /heartbeat, and only
                // so their client learns it was kicked.
                let banned_req = mid_hdr
                    .map(|id| h.banned.contains(&id))
                    .unwrap_or(false);
                if banned_req {
                    path == "/heartbeat"
                } else {
                    h.code.is_empty()
                        || given.as_deref() == Some(h.code.as_str())
                        || member_ok
                }
            }
        }
    };

    match (method.as_str(), path.as_str()) {
        ("POST", "/join") => {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let v: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
            let body_code = v["code"].as_str().unwrap_or("");
            let mut g = hs.lock().unwrap();
            let Some(h) = g.as_mut() else {
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            };
            if !h.code.is_empty() && body_code != h.code && !code_ok {
                return respond_json(req, 403, &serde_json::json!({"error": "bad join code"}));
            }
            // id 0 is reserved for the host row in member_views.
            let member_id = v["memberId"]
                .as_u64()
                .unwrap_or_else(rand_u64)
                .max(1);
            let name = v["name"].as_str().unwrap_or("member").to_string();
            // Rejoining clears a prior kick — the code is the gate again.
            h.banned.remove(&member_id);
            let appv = v["appVersion"].as_str().unwrap_or("").to_string();
            if let Some(m) = h.members.get_mut(&member_id) {
                // Same member rejoining — merge: refresh identity fields but
                // keep applied_version so they don't look "behind" until the
                // next heartbeat reports their real state.
                m.name = name.clone();
                m.last_seen = now_secs();
                m.app_version = appv;
                crate::alog!("net", "member rejoin: {name} ({member_id})");
            } else {
                h.members.insert(
                    member_id,
                    Member {
                        id: member_id,
                        name: name.clone(),
                        last_seen: now_secs(),
                        applied_version: 0,
                        app_version: appv,
                    },
                );
                crate::alog!("net", "member joined: {name} ({member_id})");
            }
            let sp = h.store_path.clone();
            save_host(&sp, h);
            respond_json(req, 200, &serde_json::json!({"ok": true, "memberId": member_id}))
        }
        ("POST", "/leave") => {
            // Member sign-off — remove their row + any targeted publish.
            let mid = member_header(&req);
            let mut g = hs.lock().unwrap();
            let Some(h) = g.as_mut() else {
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            };
            if let Some(mid) = mid {
                if let Some(m) = h.members.remove(&mid) {
                    h.targets.remove(&mid);
                    crate::alog!("net", "member left: {} ({mid})", m.name);
                    let sp = h.store_path.clone();
                    save_host(&sp, h);
                }
            }
            respond_json(req, 200, &serde_json::json!({"ok": true}))
        }
        _ if !code_ok => {
            respond_json(req, 403, &serde_json::json!({"error": "bad join code"}))
        }
        ("GET", "/manifest") => {
            let mid = member_header(&req);
            let g = hs.lock().unwrap();
            let Some(h) = g.as_ref() else {
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            };
            // A member with a targeted publish sees their own manifest.
            let m = mid.and_then(|id| h.targets.get(&id)).unwrap_or(&h.manifest);
            let v = serde_json::to_value(m).unwrap_or_default();
            respond_json(req, 200, &v)
        }
        ("GET", "/file") => {
            let rel = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("path="))
                .map(url_decode)
                .unwrap_or_default();
            // The lock is only needed to read the share folder — req.respond
            // streams the whole file, and holding the mutex across it stalled
            // every heartbeat/manifest for the duration of each transfer
            // (members flapped "host offline" while applying large packs).
            let folder = {
                let g = hs.lock().unwrap();
                match g.as_ref() {
                    None => return respond_json(req, 404, &serde_json::json!({"error": "gone"})),
                    Some(h) => h.folder.clone(),
                }
            };
            let Some(dest) = safe_rel(&folder, &rel) else {
                return respond_json(req, 400, &serde_json::json!({"error": "bad path"}));
            };
            match std::fs::File::open(&dest) {
                Ok(mut f) => {
                    use std::io::{Read, Seek, SeekFrom};
                    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                    // Range: bytes=N- → resume a .part from its offset.
                    let mut start = 0u64;
                    let mut ranged = false;
                    for hdr in req.headers() {
                        if hdr.field.equiv("Range") {
                            if let Some(v) = hdr.value.as_str().strip_prefix("bytes=") {
                                if let Some((a, _)) = v.split_once('-') {
                                    if let Ok(n) = a.trim().parse::<u64>() {
                                        start = n;
                                        ranged = true;
                                    }
                                }
                            }
                        }
                    }
                    if ranged && start >= len {
                        let resp = tiny_http::Response::empty(416).with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Range"[..],
                                format!("bytes */{len}").as_bytes(),
                            )
                            .unwrap(),
                        );
                        let _ = req.respond(resp);
                        return;
                    }
                    if ranged {
                        let _ = f.seek(SeekFrom::Start(start));
                    }
                    let remaining = len - start;
                    let mut headers = vec![
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/octet-stream"[..],
                        )
                        .unwrap(),
                        tiny_http::Header::from_bytes(
                            &b"Accept-Ranges"[..],
                            &b"bytes"[..],
                        )
                        .unwrap(),
                    ];
                    if ranged {
                        headers.push(
                            tiny_http::Header::from_bytes(
                                &b"Content-Range"[..],
                                format!("bytes {start}-{}/{len}", len.saturating_sub(1))
                                    .as_bytes(),
                            )
                            .unwrap(),
                        );
                    }
                    let reader =
                        std::io::BufReader::with_capacity(1 << 20, f).take(remaining);
                    let resp = tiny_http::Response::new(
                        tiny_http::StatusCode(if ranged { 206 } else { 200 }),
                        headers,
                        reader,
                        Some(remaining as usize),
                        None,
                    );
                    crate::alog!(
                        "xfer",
                        "serve {rel} — {remaining}B{rs}",
                        rs = if ranged { format!(" (resume@{start})") } else { String::new() }
                    );
                    let _ = req.respond(resp);
                }
                Err(_) => respond_json(req, 404, &serde_json::json!({"error": "not found"})),
            }
        }
        ("POST", "/heartbeat") => {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let v: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
            let mut g = hs.lock().unwrap();
            let Some(h) = g.as_mut() else {
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            };
            let mid = v["memberId"].as_u64();
            if let Some(mid) = mid {
                // Kicked member → tell their client to leave; don't record.
                if h.banned.contains(&mid) {
                    let members = member_views(h);
                    return respond_json(
                        req,
                        200,
                        &serde_json::json!({"kicked": true, "members": members}),
                    );
                }
                let name = v["name"].as_str().unwrap_or("member").to_string();
                let applied = v["appliedVersion"].as_u64().unwrap_or(0);
                let appv = v["appVersion"].as_str().unwrap_or("").to_string();
                if h.members.contains_key(&mid) {
                    let m = h.members.get_mut(&mid).unwrap();
                    m.last_seen = now_secs();
                    m.applied_version = applied;
                    m.name = name.clone();
                    m.app_version = appv.clone();
                } else {
                    // A heartbeat is enough to re-register — roster survives
                    // this insert via save_host, so restarts keep the row.
                    h.members.insert(
                        mid,
                        Member {
                            id: mid,
                            name: name.clone(),
                            last_seen: now_secs(),
                            applied_version: applied,
                            app_version: appv,
                        },
                    );
                    crate::alog!("net", "member re-registered via heartbeat: {name} ({mid})");
                    let sp = h.store_path.clone();
                    save_host(&sp, h);
                }
                // Applied a targeted update → retire it; the member is back on
                // the global stream.
                if let Some(t) = h.targets.get(&mid) {
                    if applied >= t.version {
                        h.targets.remove(&mid);
                    }
                }
            }
            // This member's effective latest = their target if any, else global.
            let effective = mid
                .and_then(|id| h.targets.get(&id))
                .unwrap_or(&h.manifest);
            let members = member_views(h);
            respond_json(
                req,
                200,
                &serde_json::json!({
                    "networkName": h.name,
                    "latestVersion": effective.version,
                    "latestNote": effective.note,
                    "members": members,
                }),
            )
        }
        _ => respond_json(req, 404, &serde_json::json!({"error": "not found"})),
    }
}

fn member_header(req: &tiny_http::Request) -> Option<u64> {
    req.headers()
        .iter()
        .find(|x| x.field.equiv(MEMBER_HEADER))
        .and_then(|x| x.value.as_str().parse::<u64>().ok())
}

fn rand_u64() -> u64 {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (n as u64) ^ ((n >> 64) as u64) ^ (std::process::id() as u64) << 32
}

// ---- udp announce ----

/// Every broadcast target we should announce on: the global limited
/// broadcast plus each up interface's directed broadcast. On multi-adapter
/// hosts (Ethernet + Radmin/ZeroTier/Hamachi), the OS picks ONE interface
/// for 255.255.255.255 — often the wrong one — so VPN peers never see the
/// announce. Directed broadcasts force a copy onto every adapter.
pub fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            let if_addrs::IfAddr::V4(ref v4) = i.addr else { continue };
            if i.is_loopback() || v4.ip.is_link_local() {
                continue;
            }
            let b = v4
                .broadcast
                .unwrap_or_else(|| v4.ip | !v4.netmask);
            if !b.is_unspecified() && !b.is_broadcast() && !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// The announce payload — built once, used by the announce loop and by
/// the instant query reply (`MMPKQ`).
fn announce_msg(h: &HostState) -> String {
    serde_json::json!({
        "magic": "MMPK1",
        "networkId": h.network_id,
        "name": h.name,
        "packName": h.pack_name,
        "machine": machine_name(),
        "port": HTTP_PORT,
        "latestVersion": h.manifest.version,
        "members": h.members.len(),
        "requiresCode": !h.code.is_empty(),
        "appVersion": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// The query responder needs the live HostState but `start_discovery_listener`
/// is a static once-per-process thread — hand it the shared cell once.
static QUERY_HOST: OnceLock<SharedHost> = OnceLock::new();

pub fn set_query_host(hs: &SharedHost) {
    let _ = QUERY_HOST.set(hs.clone());
}

/// A member broadcast `MMPKQ` ("who's there?") — answer instantly with a
/// unicast announce straight back to them. Discovery used to depend on the
/// next periodic announce (up to 15s while trayed); queries make it <1s.
/// Replies go to the asker's own listen port (47677) — directed unicast
/// also survives VPN adapters that eat broadcasts.
fn answer_query(src: std::net::SocketAddr) {
    let Some(hs) = QUERY_HOST.get() else { return };
    let msg = {
        let g = hs.lock().unwrap();
        g.as_ref().map(announce_msg)
    };
    let Some(msg) = msg else { return };
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        let _ = sock.set_broadcast(true);
        let _ = sock.send_to(msg.as_bytes(), (src.ip(), BROADCAST_PORT));
    }
}

/// Ask every reachable LAN segment who is hosting. Throttled — callers
/// (status polls, mount, unhide, manual refresh) can call freely.
pub fn query_networks() {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap();
        if g.map(|t| t.elapsed() < Duration::from_secs(3)).unwrap_or(false) {
            return;
        }
        *g = Some(std::time::Instant::now());
    }
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        let _ = sock.set_broadcast(true);
        let msg = br#"{"magic":"MMPKQ"}"#;
        for b in broadcast_targets() {
            let _ = sock.send_to(msg, (b, BROADCAST_PORT));
        }
    }
}

/// Broadcast one announce right now — used when the window un-hides so
/// peers don't wait for the slowed hidden-mode tick.
pub fn announce_now() {
    let Some(hs) = QUERY_HOST.get() else { return };
    let msg = {
        let g = hs.lock().unwrap();
        g.as_ref().map(announce_msg)
    };
    let Some(msg) = msg else { return };
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        let _ = sock.set_broadcast(true);
        for b in broadcast_targets() {
            let _ = sock.send_to(msg.as_bytes(), (b, BROADCAST_PORT));
        }
    }
}

/// `stop` is captured from THIS host instance — a task from a previous
/// hosting session must die even if `hs` already holds a new HostState
/// (a quick stop→start would otherwise leave two announcers running).
fn announce_loop(hs: SharedHost, stop: Arc<AtomicBool>) {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = sock.set_broadcast(true);
    loop {
        // Slowed — not stopped — while the window hides in the tray:
        // peers can still find and reach this host.
        std::thread::sleep(Duration::from_secs(if APP_HIDDEN.load(Ordering::Relaxed) {
            15
        } else {
            2
        }));
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Retry-bind in case the port was busy when hosting (re)started.
        if !server_running() {
            let _ = ensure_server(&hs);
        }
        let g = hs.lock().unwrap();
        let Some(h) = g.as_ref() else { return };
        let msg = announce_msg(h);
        drop(g);
        for b in broadcast_targets() {
            let _ = sock.send_to(msg.as_bytes(), (b, BROADCAST_PORT));
        }
    }
}

// ------------------------------------------------------- discovery (client)

static FOUND: OnceLock<Mutex<HashMap<String, FoundNetwork>>> = OnceLock::new();

fn found_map() -> &'static Mutex<HashMap<String, FoundNetwork>> {
    FOUND.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fired when an announce reveals something new — a new network, a version
/// bump, a changed addr or member count. Lets the UI refresh instantly
/// instead of waiting for its next poll.
static FOUND_HOOK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn set_found_hook(f: impl Fn() + Send + Sync + 'static) {
    let _ = FOUND_HOOK.set(Box::new(f));
}

/// Start the UDP announce listener once per process.
pub fn start_discovery_listener() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        // SO_REUSEADDR so two app instances on one PC can both listen.
        let sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        );
        let Ok(sock) = sock else { return };
        let _ = sock.set_reuse_address(true);
        #[cfg(unix)]
        let _ = sock.set_reuse_port(true);
        let addr: std::net::SocketAddr =
            format!("0.0.0.0:{BROADCAST_PORT}").parse().unwrap();
        if sock.bind(&addr.into()).is_err() {
            return;
        }
        let sock: UdpSocket = sock.into();
        let mut buf = [0u8; 4096];
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else {
                continue;
            };
            // One socket owns the port and demuxes by magic — a second
            // listener would starve one of the two on Windows, where
            // SO_REUSEADDR never duplicates datagrams across sockets.
            match v["magic"].as_str() {
                Some("MMPKAPP") => crate::appupdate::note_seeder_announce(&v, src.ip()),
                Some("MMPK1") => {}
                // "who's there?" — a host answers instantly; members ignore.
                Some("MMPKQ") => answer_query(src),
                _ => continue,
            }
            if v["magic"].as_str() != Some("MMPK1") {
                continue;
            }
            let port = v["port"].as_u64().unwrap_or(HTTP_PORT as u64);
            let f = FoundNetwork {
                network_id: v["networkId"].as_str().unwrap_or("").to_string(),
                name: v["name"].as_str().unwrap_or("?").to_string(),
                pack_name: v["packName"].as_str().unwrap_or("?").to_string(),
                machine: v["machine"].as_str().unwrap_or("?").to_string(),
                addr: format!("{}:{port}", src.ip()),
                latest_version: v["latestVersion"].as_u64().unwrap_or(0),
                members: v["members"].as_u64().unwrap_or(0) as usize,
                app_version: v["appVersion"].as_str().unwrap_or("").to_string(),
                requires_code: v["requiresCode"].as_bool(),
                last_seen: now_secs(),
                live: true,
            };
            if !f.network_id.is_empty() {
                let mut m = found_map().lock().unwrap();
                // A live announce revives a stale remembered entry too.
                let changed = match m.get(&f.network_id) {
                    Some(o) => {
                        !o.live
                            || o.latest_version != f.latest_version
                            || o.addr != f.addr
                            || o.name != f.name
                            || o.members != f.members
                    }
                    None => true,
                };
                m.insert(f.network_id.clone(), f);
                drop(m);
                if changed {
                    save_found();
                    if let Some(h) = FOUND_HOOK.get() {
                        h();
                    }
                }
            }
        }
    });
}

/// Load remembered networks from the store once — they come back stale
/// (`live: false`) so the found list isn't empty on a cold start while
/// the first query answers are still in flight.
fn seed_found_once() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let store = load_store(&effective_store_path());
    seed_found(store.found);
}

/// Seed stale entries into the found map — public for the sandbox.
#[doc(hidden)]
pub fn seed_found(found: Vec<FoundNetwork>) {
    let now = now_secs();
    let mut m = found_map().lock().unwrap();
    for mut f in found {
        f.live = false;
        if now.saturating_sub(f.last_seen) <= STALE_TTL_SECS {
            m.entry(f.network_id.clone()).or_insert(f);
        }
    }
}

/// Snapshot the found map into the store — called from the listener when
/// an announce reveals something new or a stale entry gets re-confirmed.
fn save_found() {
    let found: Vec<FoundNetwork> = found_map().lock().unwrap().values().cloned().collect();
    if found.is_empty() {
        return;
    }
    update_store(&effective_store_path(), |store| store.found = found);
}

pub fn found_networks() -> Vec<FoundNetwork> {
    start_discovery_listener();
    seed_found_once();
    // Every lookup also pings the LAN — throttled inside query_networks.
    query_networks();
    let now = now_secs();
    let mut m = found_map().lock().unwrap();
    // Live entries die when their announces stop; stale remembered entries
    // get a much longer leash — "last seen" beats "gone" for a host that's
    // just temporarily down.
    let before = m.len();
    m.retain(|_, f| {
        let ttl = if f.live { FOUND_TTL_SECS } else { STALE_TTL_SECS };
        now.saturating_sub(f.last_seen) <= ttl
    });
    if m.len() != before {
        crate::alog!("net", "dropped {} stale found entr(ies)", before - m.len());
    }
    let mut v: Vec<FoundNetwork> = m.values().cloned().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Sandbox hook — inject a found entry with a chosen last_seen so TTL
/// expiry can be asserted without waiting on real announces.
#[doc(hidden)]
pub fn found_insert_for_test(f: FoundNetwork) {
    found_map().lock().unwrap().insert(f.network_id.clone(), f);
}

/// Sandbox hook — empty the found map.
#[doc(hidden)]
pub fn found_clear_for_test() {
    found_map().lock().unwrap().clear();
}

/// Re-resolve a joined host's address from the latest announce (IP may change).
fn refresh_host_addr(c: &mut ClientState) {
    if let Some(f) = found_map().lock().unwrap().get(&c.network_id) {
        c.host_addr = f.addr.clone();
    }
}

// ------------------------------------------------------- client

pub(crate) fn synced_root() -> Result<PathBuf> {
    Ok(launchers::instances_root()?.join("_synced"))
}

pub async fn join_network(
    cs: &SharedClient,
    store_path: &Path,
    synced_root: &Path,
    addr: &str,
    code: &str,
    display_name: &str,
) -> Result<String> {
    start_discovery_listener();
    // Bare IP → default port.
    let addr_owned;
    let addr = if addr.contains(':') {
        addr
    } else {
        addr_owned = format!("{addr}:{HTTP_PORT}");
        &addr_owned
    };
    let http = reqwest::Client::new();
    let info: serde_json::Value = http
        .get(format!("http://{addr}/info"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("host unreachable")?
        .error_for_status()
        .context("host returned error")?
        .json()
        .await?;
    let network_id = info["networkId"].as_str().unwrap_or("").to_string();
    // Rejoining the SAME network reuses the saved member id — a fresh id
    // would mint a duplicate roster row on the host. Its applied_version
    // restores too so files already on disk don't look pending.
    let saved = load_store(store_path).client;
    let same_net = saved
        .as_ref()
        .map(|s| s.network_id == network_id)
        .unwrap_or(false);
    let member_id = saved
        .as_ref()
        .filter(|_| same_net)
        .map(|s| s.member_id.max(1))
        .unwrap_or_else(|| rand_u64().max(1));
    let res = http
        .post(format!("http://{addr}/join"))
        .header(CODE_HEADER, code)
        .json(&serde_json::json!({
            "code": code, "memberId": member_id, "name": display_name,
            "appVersion": env!("CARGO_PKG_VERSION")
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("join request failed")?;
    if res.status() == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("wrong join code");
    }
    let res = res.error_for_status().context("join rejected")?;
    // The host may override the id (e.g. merge/reshuffle) — adopt it.
    let member_id = res
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v["memberId"].as_u64())
        .unwrap_or(member_id)
        .max(1);

    // Switching networks? Tell the old host we're leaving before replacing
    // the session — otherwise our row ghosts on their member list forever.
    let old_leave = {
        let g = cs.lock().unwrap();
        g.as_ref().and_then(|old| {
            (old.network_id != network_id).then(|| {
                (old.host_addr.clone(), old.code.clone(), old.member_id)
            })
        })
    };

    let synced_dir = synced_root.join(&network_id);
    std::fs::create_dir_all(synced_dir.join("mods"))?;

    {
        let mut g = cs.lock().unwrap();
        *g = Some(ClientState {
            network_id: network_id.clone(),
            network_name: info["name"].as_str().unwrap_or("?").to_string(),
            host_addr: addr.to_string(),
            code: code.to_string(),
            member_id,
            display_name: display_name.to_string(),
            applied_version: saved
                .as_ref()
                .filter(|_| same_net)
                .map(|s| s.applied_version)
                .unwrap_or(0),
            declined: saved
                .as_ref()
                .filter(|_| same_net)
                .map(|s| s.declined.clone())
                .unwrap_or_default(),
            synced_dir,
            host_online: true,
            consec_failures: 0,
            applying: false,
            kicked: false,
            member_views: vec![],
            latest_version: info["latestVersion"].as_u64().unwrap_or(0),
            latest_note: String::new(),
            hash_memo: HashMap::new(),
            hb_token: rand_u64(),
        });
        save_client(store_path, g.as_ref().unwrap());
    }
    if let Some((oa, oc, om)) = old_leave {
        crate::alog!("net", "leaving previous network before rejoin");
        let _ = http
            .post(format!("http://{oa}/leave"))
            .header(CODE_HEADER, oc)
            .header(MEMBER_HEADER, om.to_string())
            .timeout(Duration::from_secs(2))
            .send()
            .await;
    }
    crate::alog!(
        "net",
        "joined \"{}\" ({network_id}) as {display_name} ({member_id})",
        info["name"].as_str().unwrap_or("?")
    );
    spawn_heartbeat(cs.clone());
    Ok(format!(
        "✓ joined \"{}\" — latest version: v{}",
        info["name"].as_str().unwrap_or("?"),
        info["latestVersion"].as_u64().unwrap_or(0)
    ))
}

/// One heartbeat POST — shared by the loop and the banned-member probe.
async fn heartbeat_once(
    http: &reqwest::Client,
    addr: &str,
    code: &str,
    mid: u64,
    name: &str,
    applied: u64,
) -> Option<serde_json::Value> {
    let r = http
        .post(format!("http://{addr}/heartbeat"))
        .header(CODE_HEADER, code)
        .header(MEMBER_HEADER, mid.to_string())
        .json(&serde_json::json!({
            "memberId": mid, "name": name, "appliedVersion": applied,
            "appVersion": env!("CARGO_PKG_VERSION")
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// Track consecutive host-contact results — `host_online` only drops
/// after OFFLINE_MISSES straight misses so one lost beat can't flap it.
fn mark_host_contact(cs: &SharedClient, ok: bool) {
    if let Some(c) = cs.lock().unwrap().as_mut() {
        if ok {
            c.consec_failures = 0;
            c.host_online = true;
        } else {
            c.consec_failures = c.consec_failures.saturating_add(1);
            if c.consec_failures >= OFFLINE_MISSES {
                c.host_online = false;
            }
        }
    }
}

fn spawn_heartbeat(cs: SharedClient) {
    let my_token = cs
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.hb_token)
        .unwrap_or(0);
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let mut wake_rx = hb_wake_gen().subscribe();
        loop {
            let (addr, code, mid, name, applied, nid) = {
                let g = cs.lock().unwrap();
                let Some(c) = g.as_ref() else { return };
                if c.hb_token != my_token {
                    return; // a newer join spawned a newer heartbeat loop
                }
                (
                    c.host_addr.clone(),
                    c.code.clone(),
                    c.member_id,
                    c.display_name.clone(),
                    c.applied_version,
                    c.network_id.clone(),
                )
            };
            let parsed = heartbeat_once(&http, &addr, &code, mid, &name, applied).await;
            {
                let mut g = cs.lock().unwrap();
                let Some(c) = g.as_mut() else { return };
                if c.hb_token != my_token || c.network_id != nid {
                    return;
                }
                match parsed {
                    Some(v) => {
                        if v["kicked"] == true {
                            c.kicked = true;
                            crate::alog!("net", "heartbeat → kicked by host");
                            return; // stop heartbeating; status clears the session
                        }
                        c.consec_failures = 0;
                        c.host_online = true;
                        if let Some(n) = v["networkName"].as_str() {
                            c.network_name = n.to_string();
                        }
                        c.latest_version =
                            v["latestVersion"].as_u64().unwrap_or(c.latest_version);
                        c.latest_note =
                            v["latestNote"].as_str().unwrap_or("").to_string();
                        c.member_views = v["members"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|m| serde_json::from_value(m.clone()).ok())
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                    None => {
                        c.consec_failures = c.consec_failures.saturating_add(1);
                        if c.consec_failures >= OFFLINE_MISSES {
                            c.host_online = false;
                        }
                    }
                }
            }
            // Presence must survive efficiency mode — slowed, not stopped.
            // A wake-gen bump (window shown) ends the wait early so the
            // member reappears online immediately instead of up to 60s later.
            let _ = tokio::time::timeout(
                Duration::from_secs(if APP_HIDDEN.load(Ordering::Relaxed) {
                    60
                } else {
                    HEARTBEAT_SECS
                }),
                wake_rx.changed(),
            )
            .await;
        }
    });
}

pub async fn leave_network(cs: &SharedClient, http: &reqwest::Client, store_path: &Path) {
    // Tell the host first (best-effort) so our row doesn't ghost, then
    // take the session and drop its synced mirror — everything in it is
    // re-downloadable, so leaving shouldn't leave a dead copy on disk.
    let old = cs.lock().unwrap().take();
    if let Some(c) = &old {
        crate::alog!("net", "leaving \"{}\" ({})", c.network_name, c.network_id);
        let _ = http
            .post(format!("http://{}/leave", c.host_addr))
            .header(CODE_HEADER, &c.code)
            .header(MEMBER_HEADER, c.member_id.to_string())
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        let _ = std::fs::remove_dir_all(&c.synced_dir);
    }
    clear_saved_client(store_path);
}

/// Fetch the latest manifest and report whether it beats the applied/declined
/// versions. None means nothing pending.
pub async fn check_pending(
    cs: &SharedClient,
    http: &reqwest::Client,
) -> Result<Option<(Manifest, usize, usize)>> {
    let (addr, code, mid, dir, applied, declined) = {
        let mut g = cs.lock().unwrap();
        let Some(c) = g.as_mut() else { return Ok(None) };
        refresh_host_addr(c);
        (
            c.host_addr.clone(),
            c.code.clone(),
            c.member_id,
            c.synced_dir.clone(),
            c.applied_version,
            c.declined.clone(),
        )
    };
    let manifest: Manifest = match async {
        let r = http
            .get(format!("http://{addr}/manifest"))
            .header(CODE_HEADER, &code)
            .header(MEMBER_HEADER, mid.to_string())
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        r.error_for_status()?.json::<Manifest>().await
    }
    .await
    {
        Ok(m) => m,
        Err(e) => {
            mark_host_contact(cs, false);
            // A kicked member gets 403 on /manifest — probe the heartbeat
            // endpoint once so "you were kicked" lands immediately instead
            // of masquerading as an unreachable host until the next beat.
            let (name, applied_v) = {
                let g = cs.lock().unwrap();
                match g.as_ref() {
                    Some(c) => (c.display_name.clone(), c.applied_version),
                    None => return Err(e).context("host unreachable"),
                }
            };
            if let Some(v) =
                heartbeat_once(http, &addr, &code, mid, &name, applied_v).await
            {
                if v["kicked"] == true {
                    if let Some(c) = cs.lock().unwrap().as_mut() {
                        c.kicked = true;
                    }
                    crate::alog!("net", "manifest 403 → heartbeat probe → kicked");
                }
            }
            return Err(e).context("host unreachable");
        }
    };
    {
        let mut g = cs.lock().unwrap();
        if let Some(c) = g.as_mut() {
            c.consec_failures = 0;
            c.host_online = true;
            c.latest_version = manifest.version;
            c.latest_note = manifest.note.clone();
        }
    }
    if manifest.version <= applied || declined.contains(&manifest.version) {
        return Ok(None);
    }
    let (to_dl, to_del) = {
        let mut g = cs.lock().unwrap();
        match g.as_mut() {
            Some(c) => diff_manifest(&dir.join("mods"), &manifest, &mut c.hash_memo),
            None => (0, 0),
        }
    };
    Ok(Some((manifest, to_dl, to_del)))
}

/// Which files need downloading / deleting to mirror `manifest` in `dir`.
/// `memo` lets an unchanged file (same size+mtime) skip its re-hash — the
/// every-4s status poll otherwise sha1s the entire synced folder forever.
fn diff_manifest(
    dir: &Path,
    manifest: &Manifest,
    memo: &mut HashMap<String, (u64, u64, String)>,
) -> (usize, usize) {
    let mtime_secs = |p: &Path| -> u64 {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    let mut to_dl = 0;
    for f in &manifest.files {
        let Some(local) = safe_rel(dir, &f.path) else { continue };
        if !local.is_file() {
            to_dl += 1;
            continue;
        }
        let size = local.metadata().map(|m| m.len()).unwrap_or(0);
        let mt = mtime_secs(&local);
        let ok = if memo
            .get(&f.path)
            .map(|(s, m, h)| *s == size && *m == mt && h == &f.sha1)
            .unwrap_or(false)
        {
            true
        } else {
            match sha1_file(&local) {
                Ok(h) => {
                    memo.insert(f.path.clone(), (size, mt, h.clone()));
                    h == f.sha1
                }
                Err(_) => false,
            }
        };
        if !ok {
            to_dl += 1;
        }
    }
    let mut to_del = 0;
    let want: std::collections::HashSet<&str> =
        manifest.files.iter().map(|f| f.path.as_str()).collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut stack = vec![rd];
        while let Some(rd) = stack.pop() {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Ok(sub) = std::fs::read_dir(&p) {
                        stack.push(sub);
                    }
                } else {
                    let rel = p
                        .strip_prefix(dir)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    if !rel.is_empty() && !want.contains(rel.as_str()) && !rel.ends_with(".part") {
                        to_del += 1;
                    }
                }
            }
        }
    }
    (to_dl, to_del)
}

/// Download the pending update into the synced dir (delta, crash-safe), then
/// write pack.json. Re-checks the manifest afterwards — if the host republished
/// mid-apply, the newer version stays pending.
pub async fn apply_update(
    cs: &SharedClient,
    http: &reqwest::Client,
    store_path: &Path,
) -> Result<String> {
    apply_update_progress(cs, http, store_path, |_| {}).await
}

/// Per-file live state inside `ApplyProgress.files`.
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ApplyFileProgress {
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub retries: u32,
}

/// Rich apply progress — phase ("hashing" | "downloading" | "done" |
/// "cancelled" | "failed"), aggregate byte counts, every in-flight file,
/// and the names of files that exhausted their retries.
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ApplyProgress {
    pub phase: String,
    #[serde(rename = "doneFiles")]
    pub done_files: usize,
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "doneBytes")]
    pub done_bytes: u64,
    #[serde(rename = "totalBytes")]
    pub total_bytes: u64,
    #[serde(rename = "hashingDone")]
    pub hashing_done: usize,
    #[serde(rename = "hashingTotal")]
    pub hashing_total: usize,
    #[serde(rename = "hashingFile")]
    pub hashing_file: String,
    pub files: Vec<ApplyFileProgress>,
    pub failed: Vec<String>,
}

/// Clears the `applying` flag on every exit path — an apply in flight
/// tells connect_status to skip its manifest poll.
struct ApplyGuard<'a>(&'a SharedClient);
impl Drop for ApplyGuard<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.0.lock().unwrap().as_mut() {
            c.applying = false;
        }
    }
}

/// One file's retry+resume loop. `.part` holds downloaded bytes; a Range
/// request picks up where it left off; sha1 on the WHOLE file gates the
/// rename — a corrupt prefix fails the check and restarts clean.
async fn download_one(
    cs: &SharedClient,
    http: &reqwest::Client,
    code: &str,
    mid: u64,
    mods_dir: &Path,
    f: &ManifestFile,
    shared: &Arc<Mutex<ApplyProgress>>,
    emit: &(dyn Fn(bool) + Sync),
) -> std::result::Result<(), String> {
    use std::io::Write;
    let local = safe_rel(mods_dir, &f.path)
        .ok_or_else(|| format!("bad path {}", f.path))?;
    let tmp = local.with_extension("part");
    if let Some(p) = tmp.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // Register this file's row in the shared progress state.
    let fi = {
        let mut s = shared.lock().unwrap();
        s.files.push(ApplyFileProgress {
            name: f.path.clone(),
            done: 0,
            total: f.size,
            retries: 0,
        });
        s.files.len() - 1
    };

    let mut last_err = String::from("no attempts");
    for attempt in 1..=FILE_RETRIES {
        if APPLY_CANCEL.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let resume_at = tmp
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
            .min(f.size);
        {
            let mut s = shared.lock().unwrap();
            s.files[fi].retries = (attempt - 1) as u32;
            s.files[fi].done = resume_at;
        }
        emit(true); // file-start is a boundary — always visible
        crate::alog!(
            "xfer",
            "file {} — attempt {attempt}/{FILE_RETRIES}{}",
            f.path,
            if resume_at > 0 { format!(" resume@{resume_at}") } else { String::new() }
        );

        // Re-resolve the host addr between attempts — VPN IPs move.
        let addr = {
            let mut g = cs.lock().unwrap();
            g.as_mut()
                .map(|c| {
                    refresh_host_addr(c);
                    c.host_addr.clone()
                })
                .unwrap_or_default()
        };
        if addr.is_empty() {
            last_err = "session gone".into();
            break;
        }
        let t0 = std::time::Instant::now();
        let mut req = http
            .get(format!("http://{addr}/file?path={}", url_encode(&f.path)))
            .header(CODE_HEADER, code)
            .header(MEMBER_HEADER, mid.to_string())
            .timeout(Duration::from_secs(FILE_TIMEOUT_SECS));
        if resume_at > 0 {
            req = req.header("Range", format!("bytes={resume_at}-"));
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("connect: {e}");
                crate::alog!("xfer", "file {} — {last_err}", f.path);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = format!("http {}", resp.status());
            crate::alog!("xfer", "file {} — {last_err}", f.path);
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        // Server honoured Range only via 206 — a 200 (old host) restarts
        // the file from zero; never append a full body to a .part.
        let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT && resume_at > 0;
        let start = if resumed { resume_at } else { 0 };
        let mut hasher = sha1::Sha1::new();
        if resumed {
            // Hash the already-downloaded prefix so the final digest covers
            // the whole file — without this a resumed file can never verify.
            match std::fs::File::open(&tmp) {
                Ok(pf) => {
                    let mut rd = std::io::BufReader::new(pf);
                    let _ = std::io::copy(&mut rd, &mut hasher);
                }
                Err(e) => {
                    last_err = format!("part read: {e}");
                    continue;
                }
            }
        }
        let mut tmpf = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&tmp)
        {
            Ok(fh) => fh,
            Err(e) => {
                last_err = format!("part open: {e}");
                continue;
            }
        };
        let mut got = start;
        let mut stream_err: Option<String> = None;
        loop {
            if APPLY_CANCEL.load(Ordering::Relaxed) {
                stream_err = Some("cancelled".into());
                break;
            }
            // Stall guard: no chunk for CHUNK_TIMEOUT_SECS → this attempt
            // is dead, retry resumes from the .part offset.
            let next = tokio::time::timeout(
                Duration::from_secs(CHUNK_TIMEOUT_SECS),
                resp.chunk(),
            )
            .await;
            match next {
                Err(_) => {
                    stream_err = Some("stalled (no data for 60s)".into());
                    break;
                }
                Ok(Err(e)) => {
                    stream_err = Some(format!("stream: {e}"));
                    break;
                }
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => {
                    if tmpf.write_all(&chunk).is_err() {
                        stream_err = Some("disk write failed".into());
                        break;
                    }
                    hasher.update(&chunk);
                    got += chunk.len() as u64;
                    {
                        let mut s = shared.lock().unwrap();
                        s.files[fi].done = got;
                    }
                    emit(false);
                }
            }
        }
        drop(tmpf);
        if let Some(se) = stream_err {
            last_err = se;
            crate::alog!("xfer", "file {} — {last_err} at {got}B", f.path);
            if last_err == "cancelled" {
                return Err("cancelled".into());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if got != f.size {
            last_err = format!("short body ({got}/{})", f.size);
            crate::alog!("xfer", "file {} — {last_err}", f.path);
            continue;
        }
        if hex::encode(hasher.finalize()) != f.sha1 {
            // Corrupt bytes — a .part we can't trust can't be resumed.
            let _ = std::fs::remove_file(&tmp);
            {
                let mut s = shared.lock().unwrap();
                s.files[fi].done = 0;
            }
            last_err = "sha1 mismatch".into();
            crate::alog!("xfer", "file {} — sha1 mismatch, restarting", f.path);
            continue;
        }
        if let Err(e) = std::fs::rename(&tmp, &local) {
            last_err = format!("rename: {e}");
            continue;
        }
        {
            let mut s = shared.lock().unwrap();
            s.files[fi].done = f.size;
            s.done_files += 1;
        }
        let ms = t0.elapsed().as_millis().max(1);
        crate::alog!(
            "xfer",
            "file {} — done {}B in {}ms ({:.1} MB/s)",
            f.path,
            f.size,
            ms,
            f.size as f64 / (ms as f64 * 1000.0)
        );
        emit(true); // file-done boundary
        return Ok(());
    }
    {
        let mut s = shared.lock().unwrap();
        s.failed.push(f.path.clone());
    }
    crate::alog!("xfer", "file {} — FAILED after retries: {last_err}", f.path);
    Err(format!("{}: {last_err}", f.path))
}

/// Full apply engine: hashing phase → PARALLEL_FILES workers with
/// per-file retry+Range-resume → sha1-verified rename → mirror sweep →
/// pack.json. Cancellable; `.part` files survive for the next apply.
pub async fn apply_update_progress(
    cs: &SharedClient,
    http: &reqwest::Client,
    store_path: &Path,
    progress: impl Fn(&ApplyProgress) + Sync,
) -> Result<String> {
    APPLY_CANCEL.store(false, Ordering::Relaxed);
    let (code, mid, dir, nid) = {
        let mut g = cs.lock().unwrap();
        let c = g.as_mut().context("not joined to a network")?;
        c.applying = true;
        refresh_host_addr(c);
        (
            c.code.clone(),
            c.member_id,
            c.synced_dir.clone(),
            c.network_id.clone(),
        )
    };
    let _apply_guard = ApplyGuard(cs);
    let addr = cs
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.host_addr.clone())
        .unwrap_or_default();
    crate::alog!("xfer", "apply start — fetching manifest from {addr}");
    let manifest: Manifest = http
        .get(format!("http://{addr}/manifest"))
        .header(CODE_HEADER, &code)
        .header(MEMBER_HEADER, mid.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("host unreachable")?
        .error_for_status()?
        .json()
        .await?;
    crate::alog!(
        "xfer",
        "manifest v{} — {} file(s)",
        manifest.version,
        manifest.files.len()
    );

    let mods_dir = dir.join("mods");
    std::fs::create_dir_all(&mods_dir)?;
    let mut log = String::new();

    let shared = Arc::new(Mutex::new(ApplyProgress {
        phase: "hashing".into(),
        hashing_total: manifest.files.len(),
        ..Default::default()
    }));
    let last_emit = Mutex::new(std::time::Instant::now() - Duration::from_secs(1));
    let emit = |force: bool| {
        let mut t = last_emit.lock().unwrap();
        if force || t.elapsed().as_millis() >= EMIT_MS {
            *t = std::time::Instant::now();
            let mut snap = shared.lock().unwrap().clone();
            drop(t);
            // Aggregate bytes = sum over every file row — resume offsets,
            // restarts and parallel workers can't skew it.
            snap.done_bytes = snap.files.iter().map(|x| x.done).sum();
            progress(&snap);
        }
    };

    // Hashing phase — sha1 every local file to build the queue. This used
    // to be invisible UI-frozen seconds; now it reports like a transfer.
    let mut queue: Vec<ManifestFile> = Vec::new();
    for (i, f) in manifest.files.iter().enumerate() {
        {
            let mut s = shared.lock().unwrap();
            s.hashing_done = i + 1;
            s.hashing_file = f.path.clone();
        }
        emit(false);
        let Some(local) = safe_rel(&mods_dir, &f.path) else { continue };
        let ok = local
            .is_file()
            .then(|| sha1_file(&local).ok())
            .flatten()
            .map(|h| h == f.sha1)
            .unwrap_or(false);
        if !ok {
            queue.push(f.clone());
        }
    }
    let total_bytes: u64 = queue.iter().map(|f| f.size).sum();
    crate::alog!(
        "xfer",
        "queue built — {} to download, {}B total",
        queue.len(),
        total_bytes
    );
    {
        let mut s = shared.lock().unwrap();
        s.total_files = queue.len();
        s.total_bytes = total_bytes;
        s.hashing_file.clear();
        s.phase = "downloading".into();
    }
    emit(true);

    if APPLY_CANCEL.load(Ordering::Relaxed) {
        shared.lock().unwrap().phase = "cancelled".into();
        emit(true);
        return Ok("! cancelled — no files downloaded yet".into());
    }

    // Parallel workers — each file gets its own retry/resume loop.
    use futures::stream::{self, StreamExt};
    let results: Vec<std::result::Result<(), String>> = stream::iter(
        queue.into_iter().map(|f| {
            let (shared, emit, code, mods_dir) = (&shared, &emit, &code, &mods_dir);
            async move {
                download_one(cs, http, code, mid, mods_dir, &f, shared, emit)
                    .await
            }
        }),
    )
    .buffer_unordered(PARALLEL_FILES)
    .collect()
    .await;
    let failures: Vec<String> = results.into_iter().filter_map(|r| r.err()).collect();

    if APPLY_CANCEL.load(Ordering::Relaxed)
        || failures.iter().any(|f| f.contains("cancelled"))
    {
        shared.lock().unwrap().phase = "cancelled".into();
        emit(true);
        crate::alog!("xfer", "apply cancelled — .part files kept for resume");
        return Ok("! cancelled — .part files kept; apply again to resume".into());
    }
    if !failures.is_empty() {
        shared.lock().unwrap().phase = "failed".into();
        emit(true);
        crate::alog!("xfer", "apply failed — {} file(s): {}", failures.len(), failures.join(", "));
        anyhow::bail!(
            "{} file(s) failed to download — the rest are in place; apply again to retry:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    // Mirror: remove local files not in the manifest. `.part` files whose
    // target IS in the manifest survive — they're resumable progress.
    let want: std::collections::HashSet<&str> =
        manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut removed = 0usize;
    let mut stack = vec![mods_dir.clone()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(&mods_dir)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    if rel.ends_with(".part") {
                        if !want.contains(&rel[..rel.len() - 5]) {
                            let _ = std::fs::remove_file(&p);
                        }
                    } else if !want.contains(rel.as_str()) {
                        let _ = std::fs::remove_file(&p);
                        removed += 1;
                    }
                }
            }
        }
    }
    if removed > 0 {
        log.push_str(&format!("✓ removed {} outdated file(s)\n", removed));
    }

    std::fs::write(
        dir.join("pack.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "networkId": nid,
            "name": manifest.pack_name,
            "version": manifest.version,
            "packVersion": manifest.pack_version,
            "mcVersion": manifest.mc_version,
            "loader": manifest.loader,
            "loaderVersion": manifest.loader_version,
        }))?,
    )?;

    {
        let mut g = cs.lock().unwrap();
        if let Some(c) = g.as_mut() {
            c.applied_version = manifest.version;
            c.declined.retain(|v| *v > manifest.version);
            c.hash_memo.clear(); // contents changed — next diff re-hashes
            save_client(store_path, c);
        }
    }
    {
        let mut s = shared.lock().unwrap();
        s.phase = "done".into();
    }
    emit(true);

    // Host may have republished mid-apply — if so, stay pending.
    if let Ok(Some((newer, _, _))) = check_pending(cs, http).await {
        if newer.version > manifest.version {
            log.push_str(&format!(
                "! applied v{}, but v{} is already out — check again",
                manifest.version, newer.version
            ));
            crate::alog!("xfer", "applied v{} but v{} already out", manifest.version, newer.version);
            return Ok(log);
        }
    }
    crate::alog!("xfer", "applied v{} complete", manifest.version);
    log.push_str(&format!(
        "✓ synced to v{} — pick a launcher below to install it",
        manifest.version
    ));
    Ok(log)
}

pub fn decline_update(cs: &SharedClient, store_path: &Path, version: u64) -> Result<()> {
    let mut g = cs.lock().unwrap();
    let c = g.as_mut().context("not joined")?;
    if !c.declined.contains(&version) {
        c.declined.push(version);
    }
    save_client(store_path, c);
    Ok(())
}

fn url_encode(s: &str) -> String {
    s.as_bytes()
        .iter()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                (*b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ------------------------------------------------------- synced packs → loader

pub fn list_synced_packs(root: &Path) -> Vec<SyncedPackInfo> {
    let base = root.join("_synced");
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(&base) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let meta = dir.join("pack.json");
        if !meta.is_file() {
            continue;
        }
        let Some(v) = std::fs::read_to_string(&meta)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            continue;
        };
        let file_count = std::fs::read_dir(dir.join("mods"))
            .map(|r| r.flatten().filter(|x| x.path().is_file()).count())
            .unwrap_or(0);
        out.push(SyncedPackInfo {
            network_id: v["networkId"].as_str().unwrap_or("").to_string(),
            name: v["name"].as_str().unwrap_or("Synced Pack").to_string(),
            version: v["version"].as_u64().unwrap_or(0),
            mc_version: v["mcVersion"].as_str().unwrap_or("").to_string(),
            loader: v["loader"].as_str().unwrap_or("").to_string(),
            loader_version: v["loaderVersion"].as_str().unwrap_or("").to_string(),
            file_count,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Load a synced pack into a launcher (copy jars + loader + profile/instance).
pub async fn load_synced_pack(
    client: &crate::modrinth::ModrinthClient,
    root: &Path,
    network_id: &str,
    launcher: &LauncherInfo,
) -> Result<String> {
    let dir = root.join("_synced").join(network_id);
    let meta = std::fs::read_to_string(dir.join("pack.json")).context("pack.json missing — apply the update first")?;
    let v: serde_json::Value = serde_json::from_str(&meta)?;
    let pack_version = v["packVersion"].as_str().unwrap_or("");
    let mut cfg = pack::PackConfig {
        name: v["name"].as_str().unwrap_or("Synced Pack").to_string(),
        version: if pack_version.is_empty() {
            format!("v{}", v["version"].as_u64().unwrap_or(0))
        } else {
            pack_version.to_string()
        },
        summary: String::new(),
        mc_version: v["mcVersion"].as_str().unwrap_or("").to_string(),
        loader: v["loader"].as_str().unwrap_or("fabric").to_string(),
        loader_version: v["loaderVersion"].as_str().unwrap_or("").to_string(),
        selected_paths: vec![],
    };
    if cfg.mc_version.is_empty() {
        anyhow::bail!("this synced pack has no MC version — host needs to set one in Build");
    }
    // Hosts may publish without a loader version — resolve latest so every
    // install path (esp. Prism's mmc-pack.json) gets a concrete one.
    if cfg.loader_version.is_empty() {
        if let Some(lv) = client
            .loader_versions(&cfg.loader, &cfg.mc_version)
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
        {
            cfg.loader_version = lv;
        }
    }
    launchers::deploy_mods_dir(client, &dir.join("mods"), &cfg, launcher, root).await
}

/// The host's "apply" — they already have the files, so deploy their live
/// folder straight into a launcher. Whatever they last published (name,
/// versions) is the meta; same deploy path members' applied packs take.
pub async fn load_host_pack(
    hs: &SharedHost,
    client: &crate::modrinth::ModrinthClient,
    root: &Path,
    launcher: &LauncherInfo,
) -> Result<String> {
    let (folder, mut cfg) = {
        let g = hs.lock().unwrap();
        let h = g.as_ref().context("not hosting")?;
        if h.manifest.version == 0 {
            anyhow::bail!("nothing published yet — publish the pack first");
        }
        let mut pack_version = h.pack_version.clone();
        if pack_version.is_empty() {
            pack_version = format!("v{}", h.manifest.version);
        }
        (
            h.folder.clone(),
            pack::PackConfig {
                name: h.pack_name.clone(),
                version: pack_version,
                summary: String::new(),
                mc_version: h.mc_version.clone(),
                loader: h.loader.clone(),
                loader_version: h.loader_version.clone(),
                selected_paths: vec![],
            },
        )
    };
    if cfg.loader_version.is_empty() {
        if let Some(lv) = client
            .loader_versions(&cfg.loader, &cfg.mc_version)
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
        {
            cfg.loader_version = lv;
        }
    }
    launchers::deploy_mods_dir(client, &folder, &cfg, launcher, root).await
}

// ------------------------------------------------------- status / resume

pub async fn connect_status(
    hs: &SharedHost,
    cs: &SharedClient,
    http: &reqwest::Client,
    store_path: &Path,
) -> Result<ConnectStatus> {
    // A kicked client gets the flag once, then their session is cleared.
    let kicked_now = cs
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.kicked)
        .unwrap_or(false);
    if kicked_now {
        leave_network(cs, http, store_path).await;
    }
    if let Some(h) = hs.lock().unwrap().as_ref() {
        let members = member_views(h);
        return Ok(ConnectStatus {
            mode: "hosting".into(),
            network_name: Some(h.name.clone()),
            network_id: Some(h.network_id.clone()),
            code: Some(h.code.clone()),
            folder: Some(h.folder.to_string_lossy().into_owned()),
            pack_name: Some(h.pack_name.clone()),
            pack_version: Some(h.pack_version.clone()),
            mc_version: Some(h.mc_version.clone()),
            loader: Some(h.loader.clone()),
            loader_version: Some(h.loader_version.clone()),
            meta_folder: h.meta_folder.clone(),
            latest_version: h.manifest.version,
            history: h
                .history
                .iter()
                .rev()
                .map(|(v, n, t)| {
                    serde_json::json!({"version": v, "note": n, "ts": t})
                })
                .collect(),
            host_addr: None,
            host_online: true,
            applied_version: h.manifest.version,
            display_name: None,
            members,
            pending: None,
            kicked: false,
        });
    }
    if cs.lock().unwrap().is_some() {
        // Mid-apply: don't poll /manifest — the download is already
        // saturating the link and the progress events carry the truth.
        let applying = cs
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.applying)
            .unwrap_or(false);
        let pending = if applying {
            None
        } else {
            check_pending(cs, http)
                .await
                .ok()
                .flatten()
                .map(|(m, dl, del)| PendingUpdate {
                    version: m.version,
                    pack_name: m.pack_name.clone(),
                    note: m.note.clone(),
                    file_count: m.files.len(),
                    to_download: dl,
                    to_delete: del,
                })
        };
        // The check_pending probe may have just learned we were kicked —
        // clear the session now rather than next poll.
        if cs
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.kicked)
            .unwrap_or(false)
        {
            leave_network(cs, http, store_path).await;
            return Ok(ConnectStatus {
                kicked: true,
                ..idle_status()
            });
        }
        let g = cs.lock().unwrap();
        let c = g.as_ref().unwrap();
        let members: Vec<MemberView> = c.member_views.clone();
        // Pack name for display/guide text: the applied pack.json is the
        // truth post-install; before that, the host's announce says it.
        let pack_name = std::fs::read_to_string(c.synced_dir.join("pack.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["name"].as_str().map(|x| x.to_string()))
            .or_else(|| {
                found_map()
                    .lock()
                    .unwrap()
                    .get(&c.network_id)
                    .map(|f| f.pack_name.clone())
            })
            .filter(|s| !s.is_empty());
        return Ok(ConnectStatus {
            mode: "joined".into(),
            network_name: Some(c.network_name.clone()),
            network_id: Some(c.network_id.clone()),
            code: None,
            folder: None,
            pack_name,
            pack_version: None,
            mc_version: None,
            loader: None,
            loader_version: None,
            meta_folder: None,
            latest_version: c.latest_version,
            history: vec![],
            host_addr: Some(c.host_addr.clone()),
            host_online: c.host_online,
            applied_version: c.applied_version,
            display_name: Some(c.display_name.clone()),
            members,
            pending,
            kicked: false,
        });
    }
    let mut st = idle_status();
    st.kicked = kicked_now;
    Ok(st)
}

fn idle_status() -> ConnectStatus {
    ConnectStatus {
        mode: "idle".into(),
        network_name: None,
        network_id: None,
        code: None,
        folder: None,
        pack_name: None,
        pack_version: None,
        mc_version: None,
        loader: None,
        loader_version: None,
        meta_folder: None,
        latest_version: 0,
        history: vec![],
        host_addr: None,
        host_online: false,
        applied_version: 0,
        display_name: None,
        members: vec![],
        pending: None,
        kicked: false,
    }
}

/// Restore the last session: re-host (server + announce) or re-join
/// (heartbeat) from connect.json. Safe to call once at startup.
pub fn resume(
    hs: &SharedHost,
    cs: &SharedClient,
    store_path: &Path,
    synced_root: &Path,
) {
    set_query_host(hs);
    let store = load_store(store_path);
    if let Some(h) = store.host {
        let _ = host_resume(hs, store_path, &h);
    }
    if let Some(c) = store.client {
        let mut g = cs.lock().unwrap();
        if g.is_none() {
            let synced_dir = synced_root.join(&c.network_id);
            crate::alog!("net", "resumed client session \"{}\"", c.network_name);
            *g = Some(ClientState {
                network_id: c.network_id,
                network_name: c.network_name,
                host_addr: c.host_addr,
                code: c.code,
                member_id: c.member_id,
                display_name: c.display_name,
                applied_version: c.applied_version,
                declined: c.declined,
                synced_dir,
                host_online: false,
                consec_failures: 0,
                applying: false,
                kicked: false,
                latest_version: 0,
                latest_note: String::new(),
                member_views: vec![],
                hash_memo: HashMap::new(),
                hb_token: rand_u64(),
            });
            drop(g);
            spawn_heartbeat(cs.clone());
        }
    }
}

/// Fetch `/info` for a manual-IP join — the UI uses `requiresCode` to
/// decide whether to even show the code field.
pub async fn network_info(
    http: &reqwest::Client,
    addr: &str,
) -> Result<serde_json::Value> {
    let addr_owned;
    let addr = if addr.contains(':') {
        addr.to_string()
    } else {
        addr_owned = format!("{addr}:{HTTP_PORT}");
        addr_owned
    };
    let v: serde_json::Value = http
        .get(format!("http://{addr}/info"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("host unreachable")?
        .error_for_status()
        .context("host returned error")?
        .json()
        .await?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::mc_from_range;

    #[test]
    fn range_normalization() {
        assert_eq!(mc_from_range("[1.21.1]"), "1.21.1");
        assert_eq!(mc_from_range("[1.21.1,)"), "1.21.1");
        assert_eq!(mc_from_range("[1.21,1.22)"), "1.21");
        assert_eq!(mc_from_range("[1.20.6, 1.21.4]"), "1.20.6");
        assert_eq!(mc_from_range("1.21.1"), "1.21.1");
        assert_eq!(mc_from_range("(,1.22]"), "1.22");
    }
}
