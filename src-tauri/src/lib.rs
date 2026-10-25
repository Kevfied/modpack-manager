pub mod applog;
pub mod appupdate;
pub mod compat;
pub mod launchers;
pub mod modrinth;
pub mod pack;
pub mod scanner;
pub mod sync;

use compat::CompatReport;
use launchers::{DetectedLoaders, LauncherInfo};
use modrinth::{DependencyWarning, ModrinthClient, ResolvedMod};
use pack::{BuildResult, PackConfig};
use scanner::{ModInfo, ScanResult};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Manager;

struct AppState {
    client: ModrinthClient,
    /// Last scan's mods, so build/deploy commands can work off paths alone.
    mods: Mutex<Vec<ModInfo>>,
    host: sync::SharedHost,
    sync_client: sync::SharedClient,
    resumed: AtomicBool,
}

fn state_mods(state: &tauri::State<AppState>) -> Vec<ModInfo> {
    state.mods.lock().unwrap().clone()
}

#[tauri::command]
async fn scan_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<ScanResult, String> {
    use tauri::Emitter;
    let res = tauri::async_runtime::spawn_blocking(move || {
        scanner::scan_folder_progress(&path, |done, total| {
            let _ = app.emit("scan-progress", serde_json::json!({ "done": done, "total": total }));
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    *state.mods.lock().unwrap() = res.mods.clone();
    Ok(res)
}

#[tauri::command]
async fn scan_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
) -> Result<ScanResult, String> {
    use tauri::Emitter;
    let res = tauri::async_runtime::spawn_blocking(move || {
        let paths: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
        scanner::scan_paths(paths, |done, total| {
            let _ = app.emit("scan-progress", serde_json::json!({ "done": done, "total": total }));
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(res)
}

#[tauri::command]
async fn resolve_mods(
    state: tauri::State<'_, AppState>,
    mods: Vec<ModInfo>,
) -> Result<HashMap<String, ResolvedMod>, String> {
    // Store mods if frontend passes them (state stays fresh).
    *state.mods.lock().unwrap() = mods.clone();
    let hashes: Vec<String> = mods.into_iter().map(|m| m.sha1).filter(|h| !h.is_empty()).collect();
    state
        .client
        .resolve_hashes(&hashes)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn compat_report(
    mods: Vec<ModInfo>,
    resolved: HashMap<String, ResolvedMod>,
    selected_paths: Vec<String>,
) -> CompatReport {
    let sel: HashSet<String> = selected_paths.into_iter().collect();
    compat::analyze(&mods, &resolved, &sel)
}

#[tauri::command]
async fn get_game_versions(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    state.client.game_versions().await.map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct UpdateInfo {
    pub filename: String,
    #[serde(rename = "projectTitle")]
    pub project_title: Option<String>,
    #[serde(rename = "currentVersion")]
    pub current_version: Option<String>,
    #[serde(rename = "newVersion")]
    pub new_version: Option<String>,
    #[serde(rename = "downloadUrl")]
    pub download_url: Option<String>,
}

#[tauri::command]
async fn check_updates(
    state: tauri::State<'_, AppState>,
    loader: String,
    mc_version: String,
) -> Result<Vec<UpdateInfo>, String> {
    let mods = state_mods(&state);
    let hashes: Vec<String> = mods.iter().map(|m| m.sha1.clone()).filter(|h| !h.is_empty()).collect();
    let updates = state
        .client
        .updates_for(&hashes, &loader, &mc_version)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = vec![];
    for m in &mods {
        if let Some(u) = updates.get(&m.sha1) {
            out.push(UpdateInfo {
                filename: m.filename.clone(),
                project_title: u.project_title.clone().or_else(|| m.name.clone()),
                current_version: m.version.clone(),
                new_version: u.version_number.clone(),
                download_url: u.download_url.clone(),
            });
        }
    }
    Ok(out)
}

#[tauri::command]
async fn get_loader_versions(
    state: tauri::State<'_, AppState>,
    loader: String,
    mc_version: String,
) -> Result<Vec<String>, String> {
    state
        .client
        .loader_versions(&loader, &mc_version)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn build_mrpack(
    state: tauri::State<'_, AppState>,
    config: PackConfig,
    out_dir: String,
) -> Result<BuildResult, String> {
    let mods = state_mods(&state);
    pack::build_mrpack(&state.client, &mods, &config, &out_dir)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn build_zip(
    state: tauri::State<'_, AppState>,
    config: PackConfig,
    out_dir: String,
) -> Result<BuildResult, String> {
    let mods = state_mods(&state);
    tauri::async_runtime::spawn_blocking(move || pack::build_zip(&mods, &config, &out_dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn build_modlist(
    state: tauri::State<'_, AppState>,
    config: PackConfig,
    resolved: HashMap<String, ResolvedMod>,
    out_dir: String,
) -> Result<BuildResult, String> {
    let mods = state_mods(&state);
    tauri::async_runtime::spawn_blocking(move || {
        pack::build_modlist(&mods, &resolved, &config, &out_dir)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn detect_launchers() -> DetectedLoaders {
    launchers::detect()
}

#[tauri::command]
async fn load_into_launcher(
    state: tauri::State<'_, AppState>,
    launcher: LauncherInfo,
    config: PackConfig,
) -> Result<String, String> {
    let mods = state_mods(&state);
    match launcher.kind.as_str() {
        "vanilla" | "tlauncher" => {
            launchers::deploy_vanilla(&state.client, &mods, &config)
                .await
                .map_err(|e| e.to_string())
        }
        "prism" => {
            launchers::deploy_prism(
                &state.client,
                &mods,
                &config,
                launcher.path.as_deref(),
                launcher.data_dir.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown launcher kind: {other}")),
    }
}

#[tauri::command]
fn open_mrpack(path: String) -> Result<String, String> {
    launchers::open_mrpack_file(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn pack_exists(launcher: LauncherInfo, name: String) -> bool {
    launchers::pack_instance_exists(&launcher, &name)
}

#[tauri::command]
fn pack_file_info(path: String) -> Result<serde_json::Value, String> {
    launchers::pack_file_info(&path).map_err(|e| e.to_string())
}

#[tauri::command]
async fn load_pack_file(
    state: tauri::State<'_, AppState>,
    launcher: LauncherInfo,
    path: String,
    config: PackConfig,
) -> Result<String, String> {
    launchers::load_pack_file(&state.client, &launcher, &path, &config)
        .await
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------- Connect

#[tauri::command]
async fn host_start(
    state: tauri::State<'_, AppState>,
    name: String,
    code: String,
    folder: String,
    pack_name: String,
    mc_version: String,
    loader: String,
    loader_version: String,
) -> Result<String, String> {
    let host = state.host.clone();
    let sp = sync::store_path();
    tauri::async_runtime::spawn_blocking(move || {
        sync::host_start(
            &host,
            &sp,
            &name,
            &code,
            &folder,
            &pack_name,
            &mc_version,
            &loader,
            &loader_version,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn host_publish(
    state: tauri::State<'_, AppState>,
    note: String,
    target: Option<u64>,
    allow_empty: bool,
) -> Result<String, String> {
    let host = state.host.clone();
    let sp = sync::store_path();
    tauri::async_runtime::spawn_blocking(move || {
        sync::host_publish(&host, &sp, &note, target, allow_empty)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Swap the hosted mods folder without restarting the network. Returns
/// [percent of files identical to the current manifest, file count].
#[tauri::command]
async fn host_set_folder(
    state: tauri::State<'_, AppState>,
    folder: String,
) -> Result<serde_json::Value, String> {
    let host = state.host.clone();
    let sp = sync::store_path();
    let (same_pct, count) = tauri::async_runtime::spawn_blocking(move || {
        sync::host_set_folder(&host, &sp, &folder)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"samePct": same_pct, "fileCount": count}))
}

/// Rename the network and/or toggle/change the join code. Empty code = open.
/// Empty pack-meta fields leave that field unchanged.
#[tauri::command]
async fn host_update(
    state: tauri::State<'_, AppState>,
    name: String,
    code: String,
    pack_name: String,
    mc_version: String,
    loader: String,
    loader_version: String,
) -> Result<String, String> {
    let host = state.host.clone();
    let sp = sync::store_path();
    tauri::async_runtime::spawn_blocking(move || {
        sync::host_update(
            &host,
            &sp,
            &name,
            &code,
            &pack_name,
            &mc_version,
            &loader,
            &loader_version,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Best-guess pack meta for a mods folder: majority loader + newest common
/// MC version from the jars' own metadata, latest loader version from
/// Modrinth, and the folder name as the pack-name suggestion.
#[tauri::command]
async fn suggest_pack_meta(
    state: tauri::State<'_, AppState>,
    folder: String,
) -> Result<serde_json::Value, String> {
    let f = folder.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        scanner::scan_folder_progress(&f, |_, _| {})
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let (mc, loader) = sync::suggest_meta(&res.mods);
    let loader = if loader.is_empty() { "fabric".to_string() } else { loader };
    let loader_version = if mc.is_empty() {
        String::new()
    } else {
        state
            .client
            .loader_versions(&loader, &mc)
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
            .unwrap_or_default()
    };
    let pack_name = std::path::Path::new(&folder)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "My Modpack".into());
    Ok(serde_json::json!({
        "packName": pack_name,
        "mcVersion": mc,
        "loader": loader,
        "loaderVersion": loader_version,
    }))
}

/// Kick a member — they're removed and blocked until they rejoin with the code.
#[tauri::command]
fn host_kick(state: tauri::State<'_, AppState>, member_id: u64) -> Result<String, String> {
    sync::host_kick(&state.host, &sync::store_path(), member_id).map_err(|e| e.to_string())
}

/// Remove a member row WITHOUT banning — for ghosts, not for kicks.
#[tauri::command]
fn host_forget(state: tauri::State<'_, AppState>, member_id: u64) -> Result<String, String> {
    sync::host_forget(&state.host, &sync::store_path(), member_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn host_stop(state: tauri::State<'_, AppState>) {
    sync::host_stop(&state.host, &sync::store_path());
}

#[tauri::command]
fn scan_networks() -> Vec<sync::FoundNetwork> {
    sync::found_networks()
}

#[tauri::command]
async fn join_network(
    state: tauri::State<'_, AppState>,
    addr: String,
    code: String,
    display_name: String,
) -> Result<String, String> {
    let root = sync::synced_root().map_err(|e| e.to_string())?;
    sync::join_network(
        &state.sync_client,
        &sync::store_path(),
        &root,
        &addr,
        &code,
        &display_name,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn leave_network(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let http = state.client.inner_http().clone();
    sync::leave_network(&state.sync_client, &http, &sync::store_path()).await;
    Ok(())
}

/// Fetch `/info` for a manual join — the UI reads `requiresCode` to
/// decide whether to ask for a join code at all.
#[tauri::command]
async fn connect_network_info(
    state: tauri::State<'_, AppState>,
    addr: String,
) -> Result<serde_json::Value, String> {
    sync::network_info(state.client.inner_http(), &addr)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn connect_status(state: tauri::State<'_, AppState>) -> Result<sync::ConnectStatus, String> {
    // One-time session resume (re-host / re-join from connect.json).
    if !state.resumed.swap(true, Ordering::Relaxed) {
        if let Ok(root) = sync::synced_root() {
            sync::resume(&state.host, &state.sync_client, &sync::store_path(), &root);
        }
    }
    sync::connect_status(
        &state.host,
        &state.sync_client,
        state.client.inner_http(),
        &sync::store_path(),
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn apply_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    use tauri::Emitter;
    sync::apply_update_progress(
        &state.sync_client,
        state.client.inner_http(),
        &sync::store_path(),
        move |p| {
            let _ = app.emit("connect-apply-progress", p);
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Ask the running apply to stop — `.part` files stay for the next resume.
#[tauri::command]
fn cancel_apply() {
    sync::cancel_apply();
}

#[tauri::command]
fn decline_update(state: tauri::State<'_, AppState>, version: u64) -> Result<(), String> {
    sync::decline_update(&state.sync_client, &sync::store_path(), version)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_synced_packs() -> Vec<sync::SyncedPackInfo> {
    launchers::instances_root()
        .map(|r| sync::list_synced_packs(&r))
        .unwrap_or_default()
}

#[tauri::command]
async fn load_synced_pack(
    state: tauri::State<'_, AppState>,
    network_id: String,
    launcher: LauncherInfo,
) -> Result<String, String> {
    let root = launchers::instances_root().map_err(|e| e.to_string())?;
    sync::load_synced_pack(&state.client, &root, &network_id, &launcher)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------- app updates

fn update_app_dir() -> std::path::PathBuf {
    sync::store_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

#[tauri::command]
async fn app_update_status(state: tauri::State<'_, AppState>) -> Result<appupdate::AppUpdateStatus, String> {
    Ok(appupdate::app_update_status(state.client.inner_http(), &update_app_dir()).await)
}

#[tauri::command]
async fn download_app_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    use tauri::Emitter;
    let dir = update_app_dir();
    let hs = state.host.clone();
    let http = state.client.inner_http().clone();
    appupdate::download_app_update(&http, &dir, &hs, move |done, total| {
        let _ = app.emit("appupdate-progress", serde_json::json!({ "done": done, "total": total }));
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_app_update() {
    appupdate::cancel_download();
}

/// Spawn the verified installer and exit — NSIS handles the upgrade.
#[tauri::command]
fn install_app_update(app: tauri::AppHandle) -> Result<String, String> {
    let msg = appupdate::install_app_update(&update_app_dir()).map_err(|e| e.to_string())?;
    app.exit(0);
    #[allow(unreachable_code)]
    Ok(msg)
}

/// Dismiss the post-update "✓ updated" banner permanently.
#[tauri::command]
fn ack_app_update() {
    appupdate::ack_update_banner(&update_app_dir());
}

/// Open the GitHub release page for a version in the default browser.
#[tauri::command]
fn open_release_notes(version: String) -> Result<(), String> {
    // Strict charset — this string lands inside a URL.
    if version.is_empty()
        || version.len() > 32
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
    {
        return Err("bad version".into());
    }
    let url = format!("https://github.com/Kevfied/modpack-manager/releases/tag/v{version}");
    std::process::Command::new("explorer.exe")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn skip_app_version(version: String) {
    appupdate::skip_version(&update_app_dir(), &version);
}

#[tauri::command]
fn unskip_app_version() {
    appupdate::unskip_version(&update_app_dir());
}

#[tauri::command]
fn pick_update_bundle(state: tauri::State<'_, AppState>, folder: String) -> Result<String, String> {
    appupdate::pick_update_bundle(&state.host, &update_app_dir(), &folder)
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------- logging

/// Open the logs folder in Explorer so a bug report is one click away.
#[tauri::command]
fn open_logs_dir() -> Result<(), String> {
    let p = applog::path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .ok_or("logging not initialized")?;
    std::process::Command::new("explorer.exe")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The newest `n` session-log lines — Connect's "copy logs" button puts
/// them on the clipboard for a bug report.
#[tauri::command]
fn tail_log(n: usize) -> Vec<String> {
    applog::tail(n.min(500))
}

#[tauri::command]
fn set_update_host_ip(addr: String) {
    appupdate::add_manual_seeder(&addr);
}

/// Startup toggle — goes through the backend so the "configured" flag is
/// always written, protecting an explicit OFF from the first-run default.
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let res = if on {
        app.autolaunch().enable()
    } else {
        app.autolaunch().disable()
    };
    res.map_err(|e| e.to_string())?;
    let _ = std::fs::write(
        update_app_dir().join("autostart-configured"),
        if on { "1" } else { "0" },
    );
    alog!("app", "autostart {} via settings", if on { "enabled" } else { "disabled" });
    Ok(())
}

#[tauri::command]
fn update_seeders() -> Vec<appupdate::FoundSeeder> {
    appupdate::found_seeders()
}

#[tauri::command]
async fn find_missing_deps(
    state: tauri::State<'_, AppState>,
    mods: Vec<ModInfo>,
    resolved: HashMap<String, ResolvedMod>,
    selected_paths: Vec<String>,
) -> Result<Vec<DependencyWarning>, String> {
    let sel: HashSet<String> = selected_paths.into_iter().collect();
    // Only look at selected mods that resolved to a Modrinth project.
    let chosen: Vec<(&ModInfo, &ResolvedMod)> = mods
        .iter()
        .filter(|m| sel.contains(&m.path))
        .filter_map(|m| resolved.get(&m.sha1).map(|r| (m, r)))
        .collect();
    let present: HashSet<String> = chosen
        .iter()
        .filter_map(|(_, r)| r.project_id.clone())
        .collect();

    // Fetch required deps for all versions concurrently.
    let futs: Vec<_> = chosen
        .iter()
        .map(|(_, r)| (r, r.version_id.clone()))
        .filter_map(|(r, vid)| vid.map(|v| (r, v)))
        .map(|(r, vid)| {
            let client = state.client.clone();
            async move { (r, client.required_deps(&vid).await.unwrap_or_default()) }
        })
        .collect();
    let dep_lists = futures::future::join_all(futs).await;

    // Collect unique missing project ids, then resolve titles concurrently.
    let mut missing: Vec<(usize, String)> = vec![]; // (index into chosen, project_id)
    let mut seen: HashSet<String> = HashSet::new();
    for (i, (_, deps)) in dep_lists.iter().enumerate() {
        for dep in deps {
            if !present.contains(dep) && seen.insert(dep.clone()) {
                missing.push((i, dep.clone()));
            }
        }
    }
    let title_futs: Vec<_> = missing
        .iter()
        .map(|(_, pid)| {
            let client = state.client.clone();
            let pid = pid.clone();
            async move { client.project_title(&pid).await.ok().flatten() }
        })
        .collect();
    let titles = futures::future::join_all(title_futs).await;

    let warnings = missing
        .into_iter()
        .zip(titles)
        .map(|((i, dep), title)| {
            let (m, r) = chosen[i];
            DependencyWarning {
                mod_name: r
                    .project_title
                    .clone()
                    .or_else(|| m.name.clone())
                    .or_else(|| m.mod_id.clone())
                    .unwrap_or_else(|| m.filename.clone()),
                required_project: dep,
                required_title: title,
            }
        })
        .collect();
    Ok(warnings)
}

/// Real Windows "Efficiency mode": IDLE priority + EcoQoS power
/// throttling on the main process while the window sits in the tray.
/// WebView2 children aren't ours to throttle — the backend is where the
/// work happens anyway.
#[cfg(windows)]
fn set_efficiency(on: bool) {
    use windows_sys::Win32::System::Threading::*;
    unsafe {
        let p = GetCurrentProcess();
        let _ = SetPriorityClass(
            p,
            if on {
                IDLE_PRIORITY_CLASS
            } else {
                NORMAL_PRIORITY_CLASS
            },
        );
        let st = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if on {
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            } else {
                0
            },
        };
        let _ = SetProcessInformation(
            p,
            ProcessPowerThrottling,
            &st as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn set_efficiency(_on: bool) {}

/// Window hidden ↔ shown — slows backend loops, toggles EcoQoS, tells the
/// frontend to pause its polls.
fn set_window_hidden(app: &tauri::AppHandle, hidden: bool) {
    sync::set_hidden(hidden);
    set_efficiency(hidden);
    use tauri::Emitter;
    let _ = app.emit(if hidden { "window-hidden" } else { "window-shown" }, ());
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        set_window_hidden(app, false);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Single-instance FIRST — a second launch focuses the trayed
        // window instead of spawning a zombie that can't bind the port.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--tray".into()]),
        ))
        .manage(AppState {
            client: ModrinthClient::new(),
            mods: Mutex::new(vec![]),
            host: Arc::new(Mutex::new(None)),
            sync_client: Arc::new(Mutex::new(None)),
            resumed: AtomicBool::new(false),
        })
        .setup(|app| {
            // Session logging first — everything below writes to it.
            applog::init(&update_app_dir());
            alog!("app", "starting {}", env!("CARGO_PKG_VERSION"));
            // First-run default: launch at login. The flag file doubles as
            // "user configured this" — once it exists we never touch the
            // registry entry again, so an explicit OFF stays off.
            let auto_flag = update_app_dir().join("autostart-configured");
            if !auto_flag.exists() {
                use tauri_plugin_autostart::ManagerExt;
                match app.autolaunch().enable() {
                    Ok(()) => {
                        let _ = std::fs::write(&auto_flag, "1");
                        alog!("app", "autostart enabled by default");
                    }
                    Err(e) => alog!("app", "autostart default enable failed: {e}"),
                }
            }
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            // App-update channel: anchor dirs, purge stale artifacts, and
            // start the passive listener + seeder announcer.
            let dir = update_app_dir();
            appupdate::init(&dir);
            appupdate::launch_purge(&dir);
            appupdate::start_update_listener();
            let st = app.state::<AppState>();
            appupdate::start_update_announcer(st.host.clone(), dir);
            // Discovery hooks → UI events, so new networks/seeders appear
            // instantly instead of on the next poll tick.
            use tauri::Emitter;
            let h = app.handle().clone();
            sync::set_found_hook(move || {
                let _ = h.emit("networks-found", ());
            });
            let h = app.handle().clone();
            appupdate::set_seed_hook(move || {
                let _ = h.emit("appupdate-offer", ());
            });

            // Tray: X hides to the icon; real quit lives on the menu.
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            let mut tray = TrayIconBuilder::new()
                .menu(&menu)
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "show" => show_main_window(app),
                    "quit" => {
                        alog!("tray", "quit via tray menu");
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = e
                    {
                        show_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let _tray = tray.build(app)?;

            // --tray (autostart launches) → stay hidden; otherwise show.
            if let Some(w) = app.get_webview_window("main") {
                if std::env::args().any(|a| a == "--tray") {
                    alog!("tray", "launched with --tray — staying hidden");
                    let h = app.handle().clone();
                    set_window_hidden(&h, true);
                } else {
                    let _ = w.show();
                }
            }
            Ok(())
        })
        .on_window_event(|win, ev| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                // X → tray, not quit. Quit lives on the tray menu.
                api.prevent_close();
                let _ = win.hide();
                set_window_hidden(&win.app_handle(), true);
            }
        })
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            scan_files,
            resolve_mods,
            compat_report,
            get_game_versions,
            get_loader_versions,
            check_updates,
            build_mrpack,
            build_zip,
            build_modlist,
            detect_launchers,
            load_into_launcher,
            open_mrpack,
            pack_exists,
            pack_file_info,
            load_pack_file,
            find_missing_deps,
            host_start,
            host_publish,
            host_set_folder,
            host_update,
            suggest_pack_meta,
            host_kick,
            host_forget,
            host_stop,
            scan_networks,
            join_network,
            leave_network,
            connect_network_info,
            connect_status,
            apply_update,
            cancel_apply,
            decline_update,
            list_synced_packs,
            load_synced_pack,
            app_update_status,
            download_app_update,
            cancel_app_update,
            install_app_update,
            ack_app_update,
            open_release_notes,
            skip_app_version,
            unskip_app_version,
            pick_update_bundle,
            set_update_host_ip,
            set_autostart,
            update_seeders,
            open_logs_dir,
            tail_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
