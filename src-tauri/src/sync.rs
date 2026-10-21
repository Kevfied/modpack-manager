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
const ONLINE_WINDOW_SECS: u64 = 45;

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
    pub version: u64,
    pub note: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub id: u64,
    pub name: String,
    pub last_seen: u64,
    pub applied_version: u64,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize)]
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
    #[serde(rename = "appVersion")]
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberView {
    pub id: u64,
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
    pub mc_version: String,
    pub loader: String,
    pub loader_version: String,
    pub manifest: Manifest,
    /// member_id -> a manifest served only to that member (targeted publish).
    /// Versions share `counter` with the global stream so they never collide.
    pub targets: HashMap<u64, Manifest>,
    pub counter: u64,
    pub history: Vec<(u64, String, u64)>, // (version, note, ts)
    pub members: HashMap<u64, Member>,
    /// Kicked member ids — blocked from heartbeat until they rejoin.
    pub banned: HashSet<u64>,
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
    /// Host kicked us — shown once, then the session is cleared.
    pub kicked: bool,
    pub member_views: Vec<MemberView>,
    pub latest_version: u64,
    pub latest_note: String,
}

pub type SharedHost = Arc<Mutex<Option<HostState>>>;
pub type SharedClient = Arc<Mutex<Option<ClientState>>>;

// ------------------------------------------------------- persistence

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub host: Option<SavedHost>,
    pub client: Option<SavedClient>,
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
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
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
    let _ = std::fs::write(path, serde_json::to_string_pretty(store).unwrap_or_default());
}

fn save_host(store_path: &Path, h: &HostState) {
    let mut store = load_store(store_path);
    store.host = Some(SavedHost {
        name: h.name.clone(),
        code: h.code.clone(),
        folder: h.folder.to_string_lossy().into_owned(),
        network_id: h.network_id.clone(),
        pack_name: h.pack_name.clone(),
        mc_version: h.mc_version.clone(),
        loader: h.loader.clone(),
        loader_version: h.loader_version.clone(),
        manifest: Some(h.manifest.clone()),
        counter: h.counter,
        history: h.history.clone(),
        banned: h.banned.iter().copied().collect(),
    });
    save_store(store_path, &store);
}

fn save_client(store_path: &Path, c: &ClientState) {
    let mut store = load_store(store_path);
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
    save_store(store_path, &store);
}

fn clear_saved_host(store_path: &Path) {
    let mut store = load_store(store_path);
    store.host = None;
    save_store(store_path, &store);
}

fn clear_saved_client(store_path: &Path) {
    let mut store = load_store(store_path);
    store.client = None;
    save_store(store_path, &store);
}

// ------------------------------------------------------- manifest

/// Hash all jars under `folder` into a manifest file list (relative paths).
fn build_file_list(folder: &Path) -> Result<Vec<ManifestFile>> {
    let res = scanner::scan_folder_progress(&folder.to_string_lossy(), |_, _| {})?;
    let mut files: Vec<ManifestFile> = res
        .mods
        .iter()
        .filter_map(|m| {
            let rel = Path::new(&m.path).strip_prefix(folder).ok()?;
            Some(ManifestFile {
                path: rel.to_string_lossy().replace('\\', "/"),
                sha1: m.sha1.clone(),
                size: m.size,
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
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
    host_stop(hs, store_path);
    let folder_path = PathBuf::from(folder);
    if !folder_path.is_dir() {
        anyhow::bail!("not a directory: {folder}");
    }
    let n_files = build_file_list(&folder_path)?.len(); // staged count only
    let network_id = new_id();
    // STRICT: nothing is published until the Publish button is pressed.
    // The manifest starts at v0 / empty — clients see "nothing published".
    let manifest = Manifest {
        network_id: network_id.clone(),
        pack_name: pack_name.to_string(),
        version: 0,
        note: String::new(),
        mc_version: mc_version.into(),
        loader: loader.into(),
        loader_version: loader_version.into(),
        files: vec![],
    };
    let stop = Arc::new(AtomicBool::new(false));
    let st = HostState {
        network_id,
        name: name.to_string(),
        code: code.to_string(),
        folder: folder_path,
        pack_name: pack_name.into(),
        mc_version: mc_version.into(),
        loader: loader.into(),
        loader_version: loader_version.into(),
        history: vec![],
        manifest,
        targets: HashMap::new(),
        counter: 0,
        members: HashMap::new(),
        banned: HashSet::new(),
        stop: stop.clone(),
    };
    *hs.lock().unwrap() = Some(st);

    if !ensure_server(hs) {
        *hs.lock().unwrap() = None;
        anyhow::bail!("can't bind port {HTTP_PORT} — another instance is using it");
    }
    let ann_state = hs.clone();
    std::thread::spawn(move || announce_loop(ann_state));

    if let Some(h) = hs.lock().unwrap().as_ref() {
        save_host(store_path, h);
    }
    Ok(format!(
        "✓ hosting \"{name}\" — {n_files} file(s) staged, nothing published yet (code: {code})"
    ))
}

/// Re-host after an app restart: restores the last saved host verbatim —
/// same network id, same manifest, same version counter. Never re-hashes or
/// republishes the folder; members see exactly what was published before.
fn host_resume(hs: &SharedHost, saved: &SavedHost) -> Result<()> {
    if hs.lock().unwrap().is_some() {
        return Ok(());
    }
    let manifest = saved.manifest.clone().unwrap_or_else(|| Manifest {
        network_id: saved.network_id.clone(),
        pack_name: saved.pack_name.clone(),
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
        mc_version: saved.mc_version.clone(),
        loader: saved.loader.clone(),
        loader_version: saved.loader_version.clone(),
        manifest,
        targets: HashMap::new(),
        counter: saved.counter,
        history: saved.history.clone(),
        members: HashMap::new(),
        banned: saved.banned.iter().copied().collect(),
        stop: stop.clone(),
    });
    // Retry-bind happens inside announce_loop if the port is busy right now.
    let _ = ensure_server(hs);
    let ann_state = hs.clone();
    std::thread::spawn(move || announce_loop(ann_state));
    Ok(())
}

/// Host-only: rename the network and/or toggle/change the join code.
/// Empty `code` = open network. Existing members are unaffected — they
/// authenticate by member id, so a code change only gates new joins.
pub fn host_update(
    hs: &SharedHost,
    store_path: &Path,
    name: &str,
    code: &str,
) -> Result<String> {
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    h.name = if name.trim().is_empty() { "My Network".into() } else { name.trim().into() };
    h.code = code.to_string();
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
    save_host(store_path, h);
    Ok(format!("✓ kicked {name} — they can rejoin with the code"))
}

pub fn host_stop(hs: &SharedHost, store_path: &Path) {
    if let Some(h) = hs.lock().unwrap().take() {
        h.stop.store(true, Ordering::Relaxed);
    }
    clear_saved_host(store_path);
}

/// Publish the current folder. `target` = a member id → only that member sees
/// this update; `None` → everyone. Versions share one counter so targeted
/// numbers can't collide with future global publishes.
/// `allow_empty` — the UI confirms before passing true; publishing an empty
/// folder would delete every mod on every member.
pub fn host_publish(
    hs: &SharedHost,
    store_path: &Path,
    note: &str,
    target: Option<u64>,
    allow_empty: bool,
) -> Result<String> {
    let mut g = hs.lock().unwrap();
    let h = g.as_mut().context("not hosting")?;
    let files = build_file_list(&h.folder)?;
    if files.is_empty() && !allow_empty {
        anyhow::bail!("EMPTY_PACK");
    }
    // No-change guard: for a targeted publish, only compare against that
    // member's previous target — republishing the global content TO a member
    // is legitimate (e.g. they declined the global update).
    let baseline = match target {
        Some(m) => h.targets.get(&m),
        None => Some(&h.manifest),
    };
    if let Some(b) = baseline {
        if files_eq(&files, &b.files) {
            return Ok("! nothing changed since last publish".into());
        }
    }
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
            id: m.id,
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
            id: 0,
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
            let member_id = v["memberId"].as_u64().unwrap_or_else(rand_u64);
            let name = v["name"].as_str().unwrap_or("member").to_string();
            // Rejoining clears a prior kick — the code is the gate again.
            h.banned.remove(&member_id);
            h.members.insert(
                member_id,
                Member {
                    id: member_id,
                    name: name.clone(),
                    last_seen: now_secs(),
                    applied_version: 0,
                    app_version: v["appVersion"].as_str().unwrap_or("").to_string(),
                },
            );
            respond_json(req, 200, &serde_json::json!({"ok": true, "memberId": member_id}))
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
            let g = hs.lock().unwrap();
            let Some(h) = g.as_ref() else {
                return respond_json(req, 404, &serde_json::json!({"error": "gone"}));
            };
            let Some(dest) = safe_rel(&h.folder, &rel) else {
                return respond_json(req, 400, &serde_json::json!({"error": "bad path"}));
            };
            match std::fs::File::open(&dest) {
                Ok(f) => {
                    let resp = tiny_http::Response::from_file(f).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/octet-stream"[..],
                        )
                        .unwrap(),
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
                h.members
                    .entry(mid)
                    .and_modify(|m| {
                        m.last_seen = now_secs();
                        m.applied_version = applied;
                        m.name = name.clone();
                        m.app_version = appv.clone();
                    })
                    .or_insert(Member {
                        id: mid,
                        name,
                        last_seen: now_secs(),
                        applied_version: applied,
                        app_version: appv,
                    });
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

fn announce_loop(hs: SharedHost) {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = sock.set_broadcast(true);
    loop {
        std::thread::sleep(Duration::from_secs(2));
        // Retry-bind in case the port was busy when hosting (re)started.
        if !server_running() {
            let _ = ensure_server(&hs);
        }
        let g = hs.lock().unwrap();
        let Some(h) = g.as_ref() else { return };
        if h.stop.load(Ordering::Relaxed) {
            return;
        }
        let msg = serde_json::json!({
            "magic": "MMPK1",
            "networkId": h.network_id,
            "name": h.name,
            "packName": h.pack_name,
            "machine": machine_name(),
            "port": HTTP_PORT,
            "latestVersion": h.manifest.version,
            "members": h.members.len(),
            "appVersion": env!("CARGO_PKG_VERSION"),
        })
        .to_string();
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
            };
            if !f.network_id.is_empty() {
                let mut m = found_map().lock().unwrap();
                let changed = match m.get(&f.network_id) {
                    Some(o) => {
                        o.latest_version != f.latest_version
                            || o.addr != f.addr
                            || o.name != f.name
                            || o.members != f.members
                    }
                    None => true,
                };
                m.insert(f.network_id.clone(), f);
                drop(m);
                if changed {
                    if let Some(h) = FOUND_HOOK.get() {
                        h();
                    }
                }
            }
        }
    });
}

pub fn found_networks() -> Vec<FoundNetwork> {
    start_discovery_listener();
    let mut v: Vec<FoundNetwork> = found_map()
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
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
    let member_id = rand_u64();
    let res = http
        .post(format!("http://{addr}/join"))
        .header(CODE_HEADER, code)
        .json(&serde_json::json!({
            "code": code, "memberId": member_id, "name": display_name
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("join request failed")?;
    if res.status() == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("wrong join code");
    }
    res.error_for_status().context("join rejected")?;

    let network_id = info["networkId"].as_str().unwrap_or("").to_string();
    let synced_dir = synced_root.join(&network_id);
    std::fs::create_dir_all(synced_dir.join("mods"))?;

    {
        let mut g = cs.lock().unwrap();
        *g = Some(ClientState {
            network_id,
            network_name: info["name"].as_str().unwrap_or("?").to_string(),
            host_addr: addr.to_string(),
            code: code.to_string(),
            member_id,
            display_name: display_name.to_string(),
            applied_version: 0,
            declined: vec![],
            synced_dir,
            host_online: true,
            kicked: false,
            member_views: vec![],
            latest_version: info["latestVersion"].as_u64().unwrap_or(0),
            latest_note: String::new(),
        });
        save_client(store_path, g.as_ref().unwrap());
    }
    spawn_heartbeat(cs.clone());
    Ok(format!(
        "✓ joined \"{}\" — latest version: v{}",
        info["name"].as_str().unwrap_or("?"),
        info["latestVersion"].as_u64().unwrap_or(0)
    ))
}

fn spawn_heartbeat(cs: SharedClient) {
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        loop {
            let (addr, code, mid, name, applied, nid) = {
                let g = cs.lock().unwrap();
                let Some(c) = g.as_ref() else { return };
                (
                    c.host_addr.clone(),
                    c.code.clone(),
                    c.member_id,
                    c.display_name.clone(),
                    c.applied_version,
                    c.network_id.clone(),
                )
            };
            let res = http
                .post(format!("http://{addr}/heartbeat"))
                .header(CODE_HEADER, &code)
                .header(MEMBER_HEADER, mid.to_string())
                .json(&serde_json::json!({
                    "memberId": mid, "name": name, "appliedVersion": applied,
                    "appVersion": env!("CARGO_PKG_VERSION")
                }))
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            // Fully resolve the response before locking — MutexGuard isn't Send.
            let parsed: Option<serde_json::Value> = match res {
                Ok(r) if r.status().is_success() => r.json().await.ok(),
                _ => None,
            };
            {
                let mut g = cs.lock().unwrap();
                let Some(c) = g.as_mut() else { return };
                if c.network_id != nid {
                    return;
                }
                match parsed {
                    Some(v) => {
                        if v["kicked"] == true {
                            c.kicked = true;
                            return; // stop heartbeating; status clears the session
                        }
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
                    None => c.host_online = false,
                }
            }
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
        }
    });
}

pub fn leave_network(cs: &SharedClient, store_path: &Path) {
    // Take the session and drop its synced mirror — everything in it is
    // re-downloadable, so leaving shouldn't leave a dead copy on disk.
    let dir = cs.lock().unwrap().take().map(|c| c.synced_dir);
    if let Some(d) = dir {
        let _ = std::fs::remove_dir_all(&d);
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
            // Can't reach the host (down OR not hosting) — mark offline.
            if let Some(c) = cs.lock().unwrap().as_mut() {
                c.host_online = false;
            }
            return Err(e).context("host unreachable");
        }
    };
    {
        let mut g = cs.lock().unwrap();
        if let Some(c) = g.as_mut() {
            c.host_online = true;
            c.latest_version = manifest.version;
            c.latest_note = manifest.note.clone();
        }
    }
    if manifest.version <= applied || declined.contains(&manifest.version) {
        return Ok(None);
    }
    let (to_dl, to_del) = diff_manifest(&dir.join("mods"), &manifest);
    Ok(Some((manifest, to_dl, to_del)))
}

/// Which files need downloading / deleting to mirror `manifest` in `dir`.
fn diff_manifest(dir: &Path, manifest: &Manifest) -> (usize, usize) {
    let mut to_dl = 0;
    for f in &manifest.files {
        let Some(local) = safe_rel(dir, &f.path) else { continue };
        let ok = local
            .is_file()
            .then(|| sha1_file(&local).ok())
            .flatten()
            .map(|h| h == f.sha1)
            .unwrap_or(false);
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
    let (addr, code, mid, dir, nid) = {
        let mut g = cs.lock().unwrap();
        let c = g.as_mut().context("not joined to a network")?;
        refresh_host_addr(c);
        (
            c.host_addr.clone(),
            c.code.clone(),
            c.member_id,
            c.synced_dir.clone(),
            c.network_id.clone(),
        )
    };
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

    let mods_dir = dir.join("mods");
    std::fs::create_dir_all(&mods_dir)?;
    let mut log = String::new();
    let mut downloaded = 0usize;

    for f in &manifest.files {
        let Some(local) = safe_rel(&mods_dir, &f.path) else { continue };
        let ok = local
            .is_file()
            .then(|| sha1_file(&local).ok())
            .flatten()
            .map(|h| h == f.sha1)
            .unwrap_or(false);
        if ok {
            continue;
        }
        let tmp = local.with_extension("part");
        if let Some(p) = tmp.parent() {
            std::fs::create_dir_all(p)?;
        }
        let bytes = http
            .get(format!(
                "http://{addr}/file?path={}",
                url_encode(&f.path)
            ))
            .header(CODE_HEADER, &code)
            .header(MEMBER_HEADER, mid.to_string())
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("download failed")?
            .error_for_status()?
            .bytes()
            .await?;
        let got = hex::encode(sha1::Sha1::digest(&bytes));
        if got != f.sha1 {
            let _ = std::fs::remove_file(&tmp);
            log.push_str(&format!("! hash mismatch on {}, skipped\n", f.path));
            continue;
        }
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &local)?;
        downloaded += 1;
    }
    log.push_str(&format!("✓ downloaded {} file(s)\n", downloaded));

    // Mirror: remove local files not in the manifest.
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
                        let _ = std::fs::remove_file(&p);
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
            save_client(store_path, c);
        }
    }

    // Host may have republished mid-apply — if so, stay pending.
    if let Ok(Some((newer, _, _))) = check_pending(cs, http).await {
        if newer.version > manifest.version {
            log.push_str(&format!(
                "! applied v{}, but v{} is already out — check again",
                manifest.version, newer.version
            ));
            return Ok(log);
        }
    }
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

/// Mirror jars src→dest (flattened by filename), then remove any dest
/// jars the source no longer has — a pack instance we manage should match
/// the pack, not accumulate jars the host removed. Non-jar files and
/// subdirs in dest are left alone. Returns (copied, removed).
fn sync_jars(src_dir: &Path, dest_mods: &Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(dest_mods)?;
    let mut count = 0usize;
    let mut want = HashSet::new();
    let mut stack = vec![src_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jar").unwrap_or(false) {
                let name = p
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                want.insert(name.clone());
                std::fs::copy(&p, dest_mods.join(&name))?;
                count += 1;
            }
        }
    }
    let mut removed = 0usize;
    if let Ok(rd) = std::fs::read_dir(dest_mods) {
        for e in rd.flatten() {
            let p = e.path();
            let is_jar = p.is_file() && p.extension().map(|x| x == "jar").unwrap_or(false);
            let in_pack = p
                .file_name()
                .map(|f| want.contains(&f.to_string_lossy().into_owned()))
                .unwrap_or(false);
            if is_jar && !in_pack {
                let _ = std::fs::remove_file(&p);
                removed += 1;
            }
        }
    }
    Ok((count, removed))
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
    let cfg = pack::PackConfig {
        name: v["name"].as_str().unwrap_or("Synced Pack").to_string(),
        version: format!("v{}", v["version"].as_u64().unwrap_or(0)),
        summary: String::new(),
        mc_version: v["mcVersion"].as_str().unwrap_or("").to_string(),
        loader: v["loader"].as_str().unwrap_or("fabric").to_string(),
        loader_version: v["loaderVersion"].as_str().unwrap_or("").to_string(),
        selected_paths: vec![],
    };
    if cfg.mc_version.is_empty() {
        anyhow::bail!("this synced pack has no MC version — host needs to set one in Build");
    }
    let mods_dir = dir.join("mods");
    let mut log = String::new();

    match launcher.kind.as_str() {
        "prism" => {
            let dd = launcher.data_dir.as_deref().context("prism data dir not found")?;
            let id = pack::slugify(&cfg.name);
            let inst = Path::new(dd).join("instances").join(&id);
            let gamedir = inst.join(".minecraft");
            let (n, removed) = sync_jars(&mods_dir, &gamedir.join("mods"))?;
            if removed > 0 {
                log.push_str(&format!("✓ removed {removed} outdated mod(s)\n"));
            }
            std::fs::write(
                inst.join("instance.cfg"),
                format!("InstanceType=OneSix\nname={}\nicon=default\n", cfg.name),
            )?;
            std::fs::write(
                inst.join("mmc-pack.json"),
                serde_json::to_string_pretty(&serde_json::json!({
                    "formatVersion": 1,
                    "components": [
                        { "uid": "net.minecraft", "version": cfg.mc_version },
                        { "uid": launchers::prism_component_uid(&cfg.loader)?,
                          "version": cfg.loader_version, "important": true }
                    ]
                }))?,
            )?;
            log.push_str(&format!(
                "✓ wrote Prism instance \"{id}\" — {n} mod(s) ({} {})",
                cfg.loader, cfg.loader_version
            ));
        }
        _ => {
            let mc = launchers::minecraft_dir().context(".minecraft not found")?;
            let instance = root.join(pack::slugify(&cfg.name));
            let (n, removed) = sync_jars(&mods_dir, &instance.join("mods"))?;
            log.push_str(&format!("✓ staged {n} mod(s)\n"));
            if removed > 0 {
                log.push_str(&format!("✓ removed {removed} outdated mod(s)\n"));
            }
            let version_id =
                launchers::ensure_loader_version(client, &mc, &cfg, &mut log).await?;
            launchers::add_launcher_profile(&mc, &cfg.name, &version_id, &instance)?;
            log.push_str(&format!(
                "✓ profile \"{}\" ready — open your launcher, pick it, press Play",
                cfg.name
            ));
        }
    }
    Ok(log)
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
        leave_network(cs, store_path);
    }
    if let Some(h) = hs.lock().unwrap().as_ref() {
        let members = member_views(h);
        return Ok(ConnectStatus {
            mode: "hosting".into(),
            network_name: Some(h.name.clone()),
            network_id: Some(h.network_id.clone()),
            code: Some(h.code.clone()),
            folder: Some(h.folder.to_string_lossy().into_owned()),
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
        let pending = check_pending(cs, http)
            .await
            .ok()
            .flatten()
            .map(|(m, dl, del)| PendingUpdate {
                version: m.version,
                note: m.note.clone(),
                file_count: m.files.len(),
                to_download: dl,
                to_delete: del,
            });
        let g = cs.lock().unwrap();
        let c = g.as_ref().unwrap();
        let members: Vec<MemberView> = c.member_views.clone();
        return Ok(ConnectStatus {
            mode: "joined".into(),
            network_name: Some(c.network_name.clone()),
            network_id: Some(c.network_id.clone()),
            code: None,
            folder: None,
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
    Ok(ConnectStatus {
        mode: "idle".into(),
        network_name: None,
        network_id: None,
        code: None,
        folder: None,
        latest_version: 0,
        history: vec![],
        host_addr: None,
        host_online: false,
        applied_version: 0,
        display_name: None,
        members: vec![],
        pending: None,
        kicked: kicked_now,
    })
}

/// Restore the last session: re-host (server + announce) or re-join
/// (heartbeat) from connect.json. Safe to call once at startup.
pub fn resume(
    hs: &SharedHost,
    cs: &SharedClient,
    store_path: &Path,
    synced_root: &Path,
) {
    let store = load_store(store_path);
    if let Some(h) = store.host {
        let _ = host_resume(hs, &h);
    }
    if let Some(c) = store.client {
        let mut g = cs.lock().unwrap();
        if g.is_none() {
            let synced_dir = synced_root.join(&c.network_id);
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
                kicked: false,
                latest_version: 0,
                latest_note: String::new(),
                member_views: vec![],
            });
            drop(g);
            spawn_heartbeat(cs.clone());
        }
    }
}
