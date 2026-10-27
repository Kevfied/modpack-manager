use crate::modrinth::ModrinthClient;
use crate::pack::{self, PackConfig};
use crate::scanner::ModInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherInfo {
    pub kind: String, // "vanilla" | "prism" | "tlauncher"
    pub name: String,
    pub path: Option<String>,
    #[serde(rename = "dataDir")]
    pub data_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectedLoaders {
    pub launchers: Vec<LauncherInfo>,
    #[serde(rename = "installedVersions")]
    pub installed_versions: Vec<String>,
}

pub fn minecraft_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(".minecraft")).filter(|p| p.is_dir())
}

pub fn detect() -> DetectedLoaders {
    let mut launchers = vec![];
    let mut installed_versions = vec![];

    if let Some(mc) = minecraft_dir() {
        installed_versions = std::fs::read_dir(mc.join("versions"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        installed_versions.sort();

        launchers.push(LauncherInfo {
            kind: "vanilla".into(),
            name: "Minecraft Launcher (.minecraft)".into(),
            path: None,
            data_dir: Some(mc.to_string_lossy().to_string()),
        });
    }

    // TLauncher: game dir comes from .tlauncher/tlauncher-2.0.properties
    // (minecraft.gamedir), defaulting to .minecraft. Its profiles/accounts
    // file TlauncherProfiles.json lives inside the game dir.
    let tl_props = dirs::data_dir().map(|d| d.join(".tlauncher").join("tlauncher-2.0.properties"));
    let tl_installed = tl_props.as_ref().is_some_and(|p| p.is_file());
    let tl_gamedir = tl_props
        .as_ref()
        .filter(|p| p.is_file())
        .and_then(|p| read_prop(&p, "minecraft.gamedir"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(minecraft_dir);
    if let Some(dir) = tl_gamedir {
        if tl_installed || dir.join("TlauncherProfiles.json").is_file() {
            launchers.push(LauncherInfo {
                kind: "tlauncher".into(),
                name: "TLauncher".into(),
                path: None,
                data_dir: Some(dir.to_string_lossy().to_string()),
            });
        }
        // Custom game dir? Its versions matter too — merge into the picker list.
        if minecraft_dir().map(|mc| mc != dir).unwrap_or(true) {
            if let Ok(rd) = std::fs::read_dir(dir.join("versions")) {
                installed_versions.extend(
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().to_string()),
                );
                installed_versions.sort();
                installed_versions.dedup();
            }
        }
    }

    let prism_candidates = [
        dirs::data_local_dir().map(|d| d.join("Programs/PrismLauncher/prismlauncher.exe")),
        Some(PathBuf::from(r"C:\Program Files\PrismLauncher\prismlauncher.exe")),
    ];
    let prism_exe = prism_candidates.iter().flatten().find(|p| p.is_file());
    let prism_data = dirs::data_dir()
        .map(|d| d.join("PrismLauncher"))
        .filter(|p| p.is_dir());
    if prism_exe.is_some() || prism_data.is_some() {
        launchers.push(LauncherInfo {
            kind: "prism".into(),
            name: "Prism Launcher".into(),
            path: prism_exe.map(|p| p.to_string_lossy().to_string()),
            data_dir: prism_data.map(|p| p.to_string_lossy().to_string()),
        });
    }

    DetectedLoaders {
        launchers,
        installed_versions,
    }
}

pub(crate) fn instances_root() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .context("no local appdata")?
        .join("modpack-manager")
        .join("instances");
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// Read a key from a Java .properties file (unescapes `\\`, `\:`, `\=`).
fn read_prop(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}=");
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix(&prefix) {
            // Java properties escapes: \\ -> \, \: -> :, \= -> =
            let mut out = String::with_capacity(v.len());
            let mut chars = v.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some(n) => out.push(n),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            return Some(out);
        }
    }
    None
}

/// TLauncher's game dir: minecraft.gamedir from tlauncher-2.0.properties,
/// falling back to the standard .minecraft.
pub fn tlauncher_gamedir() -> Option<PathBuf> {
    dirs::data_dir()
        .map(|d| d.join(".tlauncher").join("tlauncher-2.0.properties"))
        .filter(|p| p.is_file())
        .and_then(|p| read_prop(&p, "minecraft.gamedir"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(minecraft_dir)
}

/// Folder/id name for a TLauncher modpack version. Real TLauncher packs keep
/// their display name verbatim (spaces, caps) — only strip path-illegal chars.
fn tlauncher_dirname(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    let clean = clean.trim().trim_end_matches('.').trim();
    if clean.is_empty() { "Modpack".into() } else { clean.into() }
}

/// Write versions/<name>/<name>.json + TLauncherAdditional.json so the pack
/// appears in TLauncher's version list as a self-contained modpack whose game
/// dir is the version folder itself. Returns the version dir (= game dir).
pub(crate) async fn tlauncher_stage_version(
    client: &ModrinthClient,
    mc: &Path,
    config: &PackConfig,
    log: &mut String,
) -> Result<PathBuf> {
    let dirname = tlauncher_dirname(&config.name);
    let version_dir = mc.join("versions").join(&dirname);

    // Loader profile: fetch fabric/quilt directly; reuse an installed
    // forge/neoforge version's json (can't run their installers here).
    let mut profile = match config.loader.as_str() {
        "fabric" | "quilt" => {
            let lv = if config.loader_version.is_empty() {
                client
                    .loader_versions(&config.loader, &config.mc_version)
                    .await?
                    .into_iter()
                    .next()
                    .context("no loader versions found")?
            } else {
                config.loader_version.clone()
            };
            if config.loader == "fabric" {
                client.fabric_profile(&config.mc_version, &lv).await?
            } else {
                client.quilt_profile(&config.mc_version, &lv).await?
            }
        }
        _ => {
            let vid = ensure_loader_version(client, mc, config, log).await?;
            let src = mc.join("versions").join(&vid).join(format!("{vid}.json"));
            serde_json::from_str(&std::fs::read_to_string(&src)?)
                .context("parse loader version json")?
        }
    };
    // Rename to the pack name and add the fields TLauncher appends.
    profile["id"] = serde_json::json!(dirname);
    if profile.get("jar").is_none() && !config.mc_version.is_empty() {
        profile["jar"] = serde_json::json!(config.mc_version);
    }
    profile["source"] = serde_json::json!("local_version_repo");
    if profile.get("skinVersion").is_none() {
        profile["skinVersion"] = serde_json::json!(false);
    }

    std::fs::create_dir_all(&version_dir)?;
    std::fs::write(
        version_dir.join(format!("{dirname}.json")),
        serde_json::to_string_pretty(&profile)?,
    )?;

    // TLauncher creates this on first launch — pre-create so the TL badge
    // and skin system work immediately. Leave an existing one alone (it may
    // hold modpack registry data).
    let additional = version_dir.join("TLauncherAdditional.json");
    if !additional.is_file() {
        std::fs::write(
            &additional,
            serde_json::to_string_pretty(&serde_json::json!({
                "activateSkinCapeForUserVersion": true,
                "skipHashsumValidation": false,
            }))?,
        )?;
    }
    log.push_str(&format!("✓ wrote TLauncher version \"{dirname}\"\n"));
    Ok(version_dir)
}

/// Deploy to TLauncher: a self-contained modpack folder in versions/<name>/
/// (json + mods + TLauncherAdditional.json). The folder IS the game dir.
pub async fn deploy_tlauncher(
    client: &ModrinthClient,
    mods: &[ModInfo],
    config: &PackConfig,
    data_dir: Option<&str>,
) -> Result<String> {
    let mc = data_dir
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(tlauncher_gamedir)
        .context("TLauncher game dir not found")?;
    let mut log = String::new();
    let gamedir = tlauncher_stage_version(client, &mc, config, &mut log).await?;
    let n = pack::stage_instance(&gamedir, mods, config)?;
    log.push_str(&format!(
        "✓ staged {} mod(s) into {}\n",
        n,
        gamedir.to_string_lossy()
    ));
    log.push_str(&format!(
        "✓ done — open TLauncher, pick \"{}\" in the version list, press Play",
        config.name
    ));
    Ok(log)
}

/// Deploy to .minecraft: stage instance dir, ensure loader version json,
/// add a launcher_profiles.json entry. Official launcher only — TLauncher
/// gets its own self-contained path (deploy_tlauncher).
pub async fn deploy_vanilla(
    client: &ModrinthClient,
    mods: &[ModInfo],
    config: &PackConfig,
) -> Result<String> {
    let mc = minecraft_dir().context(".minecraft not found")?;
    let mut log = String::new();
    let slug = pack::slugify(&config.name);

    // 1. Stage instance game dir.
    let instance = instances_root()?.join(&slug);
    std::fs::create_dir_all(&instance)?;
    let n = pack::stage_instance(&instance, mods, config)?;
    log.push_str(&format!(
        "✓ staged {} mod(s) into {}\n",
        n,
        instance.to_string_lossy()
    ));

    // 2. Ensure a loader version exists in .minecraft/versions.
    let version_id = ensure_loader_version(client, &mc, config, &mut log).await?;

    // 3. Write launcher profile.
    add_launcher_profile(&mc, &config.name, &version_id, &instance)?;
    log.push_str(&format!(
        "✓ profile \"{}\" added to launcher_profiles.json\n",
        config.name
    ));

    log.push_str(&format!(
        "✓ done — open your launcher, pick \"{}\", press Play (version: {version_id})",
        config.name
    ));
    Ok(log)
}

pub(crate) async fn ensure_loader_version(
    client: &ModrinthClient,
    mc: &Path,
    config: &PackConfig,
    log: &mut String,
) -> Result<String> {
    let versions_dir = mc.join("versions");
    match config.loader.as_str() {
        "fabric" | "quilt" => {
            let lv = if config.loader_version.is_empty() {
                client
                    .loader_versions(&config.loader, &config.mc_version)
                    .await?
                    .into_iter()
                    .next()
                    .context("no loader versions found")?
            } else {
                config.loader_version.clone()
            };
            let profile = if config.loader == "fabric" {
                client.fabric_profile(&config.mc_version, &lv).await?
            } else {
                client.quilt_profile(&config.mc_version, &lv).await?
            };
            let id = profile
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                anyhow::bail!("loader profile json missing id");
            }
            let dir = versions_dir.join(&id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&profile)?,
            )?;
            log.push_str(&format!("✓ installed {id} into versions/\n"));
            Ok(id)
        }
        "neoforge" | "forge" => {
            // Reuse an already-installed matching loader version if present.
            let needle = format!("{}-{}", config.loader, config.loader_version);
            let fuzzy = config.loader.clone();
            let mut candidates: Vec<String> = std::fs::read_dir(&versions_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if !config.loader_version.is_empty() {
                if let Some(hit) = candidates.iter().find(|c| c.contains(&needle)) {
                    log.push_str(&format!("✓ reusing installed version {hit}\n"));
                    return Ok(hit.clone());
                }
            }
            let mc_hint = config
                .mc_version
                .split('.')
                .nth(1)
                .map(|m| m.to_string())
                .unwrap_or_default();
            candidates.retain(|c| {
                c.to_lowercase().contains(&fuzzy)
                    && (mc_hint.is_empty() || c.contains(&mc_hint) || c.contains(&config.mc_version))
            });
            candidates.sort();
            if let Some(hit) = candidates.last() {
                log.push_str(&format!("✓ reusing installed version {hit}\n"));
                Ok(hit.clone())
            } else {
                anyhow::bail!(
                    "No {} install found in versions/. Run the {} installer for {} once (from its website), then retry.",
                    config.loader, config.loader, config.mc_version
                )
            }
        }
        other => anyhow::bail!("unsupported loader: {other}"),
    }
}

pub(crate) fn add_launcher_profile(mc: &Path, name: &str, version_id: &str, game_dir: &Path) -> Result<()> {
    let file = mc.join("launcher_profiles.json");
    let mut doc: serde_json::Value = if file.is_file() {
        // Back up before touching it.
        let _ = std::fs::copy(&file, file.with_extension("json.bak"));
        serde_json::from_str(&std::fs::read_to_string(&file)?)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if doc.get("profiles").is_none() {
        doc["profiles"] = serde_json::json!({});
    }
    let now = chrono_now();
    let key = format!("modpack-manager-{}", pack::slugify(name));
    doc["profiles"][key] = serde_json::json!({
        "name": name,
        "type": "custom",
        "created": now,
        "lastUsed": now,
        "icon": "Grass",
        "lastVersionId": version_id,
        "gameDir": game_dir.to_string_lossy().replace('\\', "\\\\"),
    });
    std::fs::write(&file, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn chrono_now() -> String {
    // RFC3339-ish timestamp without pulling in chrono.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // days -> y/m/d via civil algorithm
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Prism component uid for each loader.
pub(crate) fn prism_component_uid(loader: &str) -> Result<&'static str> {
    Ok(match loader {
        "fabric" => "net.fabricmc.fabric-loader",
        "quilt" => "org.quiltmc.quilt-loader",
        "forge" => "net.minecraftforge",
        "neoforge" => "net.neoforged",
        other => anyhow::bail!("unsupported loader for prism: {other}"),
    })
}

fn safe_dest(instance: &Path, rel: &str) -> Option<PathBuf> {
    // Reject paths that escape the instance dir.
    let rel = rel.replace('\\', "/");
    if rel.contains("..") || rel.starts_with('/') || rel.contains(':') {
        return None;
    }
    Some(instance.join(rel))
}

struct MrpackDeps {
    name: String,
    version: String,
    mc_version: String,
    loader: String,
    loader_version: String,
}

/// Materialize a .mrpack's contents (downloaded files + overrides) into
/// `parent/<slug>[/gamedir_name]`. Returns the pack's deps + instance dir.
async fn install_mrpack_files(
    path: &str,
    parent: &Path,
    gamedir_name: Option<&str>,
    pretty_name: bool,
    log: &mut String,
) -> Result<(MrpackDeps, PathBuf)> {
    use std::io::Read;
    let file = std::fs::File::open(path).context("open mrpack")?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut index_text = String::new();
    zip.by_name("modrinth.index.json")
        .context("mrpack missing modrinth.index.json")?
        .read_to_string(&mut index_text)?;
    let index: serde_json::Value = serde_json::from_str(&index_text)?;

    let pack_name = index["name"].as_str().unwrap_or("Imported Pack");
    let folder = if pretty_name {
        tlauncher_dirname(pack_name)
    } else {
        pack::slugify(pack_name)
    };
    let instance = parent.join(folder);
    let gamedir = match gamedir_name {
        Some(sub) => instance.join(sub),
        None => instance.clone(),
    };
    std::fs::create_dir_all(&gamedir)?;

    // Download referenced files (mods etc.), verifying sha1.
    let empty: Vec<serde_json::Value> = vec![];
    let files = index["files"].as_array().unwrap_or(&empty);
    let mut downloaded = 0usize;
    let http = reqwest::Client::new();
    for f in files {
        let Some(rel) = f["path"].as_str() else { continue };
        let Some(url) = f["downloads"]
            .as_array()
            .and_then(|d| d.first())
            .and_then(|u| u.as_str())
        else { continue };
        let Some(dest) = safe_dest(&gamedir, rel) else { continue };
        let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
        if let Some(want) = f["hashes"]["sha1"].as_str() {
            use sha1::Digest;
            let got = hex::encode(sha1::Sha1::digest(&bytes));
            if got != want {
                log.push_str(&format!("! hash mismatch on {rel}, skipped\n"));
                continue;
            }
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes)?;
        downloaded += 1;
    }
    log.push_str(&format!("✓ downloaded {} file(s)\n", downloaded));

    // Apply overrides into the game dir.
    let overrides: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with("overrides/") && !n.ends_with('/'))
        .map(|n| n.to_string())
        .collect();
    for entry in overrides {
        let rel = entry.trim_start_matches("overrides/");
        let Some(dest) = safe_dest(&gamedir, rel) else { continue };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut src = zip.by_name(&entry)?;
        let mut dst = std::fs::File::create(&dest)?;
        std::io::copy(&mut src, &mut dst)?;
    }
    log.push_str("✓ applied overrides\n");

    // Loader + MC version from the pack's dependencies.
    let deps = &index["dependencies"];
    let mc_version = deps["minecraft"].as_str().unwrap_or_default().to_string();
    let (loader, loader_version) = [
        ("fabric-loader", "fabric"),
        ("quilt-loader", "quilt"),
        ("forge", "forge"),
        ("neoforge", "neoforge"),
    ]
    .iter()
    .find(|(k, _)| deps.get(*k).is_some())
    .map(|(k, l)| (l.to_string(), deps[*k].as_str().unwrap_or("").to_string()))
    .unwrap_or(("fabric".into(), String::new()));

    Ok((
        MrpackDeps {
            name: index["name"].as_str().unwrap_or("Imported Pack").into(),
            version: index["versionId"].as_str().unwrap_or("1.0.0").into(),
            mc_version,
            loader,
            loader_version,
        },
        instance,
    ))
}

/// Install a .mrpack into .minecraft: download its referenced files, apply
/// overrides, install the loader version, write a launcher profile.
pub async fn install_mrpack_vanilla(
    client: &ModrinthClient,
    path: &str,
    mc: &Path,
    root: &Path,
) -> Result<String> {
    let mut log = String::new();
    let (deps, instance) = install_mrpack_files(path, root, None, false, &mut log).await?;
    let cfg = PackConfig {
        name: deps.name.clone(),
        version: deps.version,
        summary: String::new(),
        mc_version: deps.mc_version.clone(),
        loader: deps.loader.clone(),
        loader_version: deps.loader_version.clone(),
        selected_paths: vec![],
    };
    let version_id = ensure_loader_version(client, mc, &cfg, &mut log).await?;
    add_launcher_profile(mc, &deps.name, &version_id, &instance)?;
    log.push_str(&format!(
        "✓ profile \"{}\" ready in launcher (MC {}, {} {})\n",
        deps.name, deps.mc_version, deps.loader, deps.loader_version
    ));
    log.push_str("✓ instance installed — open your launcher, pick the profile, press Play");
    Ok(log)
}

/// Install a .mrpack as a TLauncher modpack: files land directly in
/// versions/<pack name>/ (the version folder IS the game dir), plus the
/// pack-named version json and TLauncherAdditional.json.
pub async fn install_mrpack_tlauncher(
    client: &ModrinthClient,
    path: &str,
    mc: &Path,
) -> Result<String> {
    let mut log = String::new();
    let versions = mc.join("versions");
    let (deps, instance) = install_mrpack_files(path, &versions, None, true, &mut log).await?;
    let cfg = PackConfig {
        name: deps.name.clone(),
        version: deps.version,
        summary: String::new(),
        mc_version: deps.mc_version.clone(),
        loader: deps.loader.clone(),
        loader_version: deps.loader_version.clone(),
        selected_paths: vec![],
    };
    let staged = tlauncher_stage_version(client, mc, &cfg, &mut log).await?;
    debug_assert_eq!(staged, instance); // same name -> same version dir
    log.push_str(&format!(
        "✓ modpack \"{}\" ready — open TLauncher, pick it, press Play",
        deps.name
    ));
    Ok(log)
}

/// Install a plain .zip into TLauncher as a self-contained versions/<name>/ pack.
pub async fn install_zip_tlauncher(
    client: &ModrinthClient,
    path: &str,
    mc: &Path,
    config: &PackConfig,
) -> Result<String> {
    let file = std::fs::File::open(path).context("open zip")?;
    let mut zip = zip::ZipArchive::new(file)?;
    if config.mc_version.is_empty() {
        anyhow::bail!("set an MC version (Build tab) before loading a .zip into TLauncher");
    }
    let dirname = tlauncher_dirname(&config.name);
    let gamedir = mc.join("versions").join(&dirname);
    let mods_dir = gamedir.join("mods");
    std::fs::create_dir_all(&mods_dir)?;

    let entries: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let mut count = 0usize;
    for entry in &entries {
        if entry.ends_with('/') {
            continue;
        }
        let is_jar = entry.to_lowercase().ends_with(".jar");
        let in_mods = entry.starts_with("mods/") || !entry.contains('/');
        if is_jar && in_mods {
            let dest = mods_dir.join(pack::safe_filename(entry));
            let mut src = zip.by_name(entry)?;
            let mut dst = std::fs::File::create(&dest)?;
            std::io::copy(&mut src, &mut dst)?;
            count += 1;
        } else if entry.starts_with("config/") {
            let rel = entry.trim_start_matches("config/");
            if let Some(dest) = safe_dest(&gamedir.join("config"), rel) {
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut src = zip.by_name(entry)?;
                let mut dst = std::fs::File::create(&dest)?;
                std::io::copy(&mut src, &mut dst)?;
            }
        }
    }
    let mut log = format!("✓ extracted {count} mod(s)\n");
    tlauncher_stage_version(client, mc, config, &mut log).await?;
    log.push_str(&format!(
        "✓ modpack \"{dirname}\" ready — open TLauncher, pick it, press Play"
    ));
    Ok(log)
}

/// Install a .mrpack as a real Prism instance (downloads + overrides into
/// instances/<id>/.minecraft, mmc-pack.json with the pack's loader deps).
pub async fn install_mrpack_prism(path: &str, data_dir: &str) -> Result<String> {
    let mut log = String::new();
    let inst_root = Path::new(data_dir).join("instances");
    let (deps, instance) =
        install_mrpack_files(path, &inst_root, Some(".minecraft"), false, &mut log).await?;
    let id = instance
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| pack::slugify(&deps.name));
    std::fs::write(
        instance.join("instance.cfg"),
        format!("InstanceType=OneSix\nname={}\nicon=default\n", deps.name),
    )?;
    let mut components = vec![serde_json::json!({
        "uid": "net.minecraft",
        "version": deps.mc_version,
    })];
    if !deps.loader_version.is_empty() {
        components.push(serde_json::json!({
            "uid": prism_component_uid(&deps.loader)?,
            "version": deps.loader_version,
            "important": true
        }));
    }
    std::fs::write(
        instance.join("mmc-pack.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "formatVersion": 1,
            "components": components
        }))?,
    )?;
    log.push_str(&format!(
        "✓ wrote Prism instance \"{id}\" (MC {}, {} {}) — open Prism and launch it",
        deps.mc_version, deps.loader, deps.loader_version
    ));
    Ok(log)
}

/// Install a plain .zip (mods-folder export) into .minecraft using the
/// shared pack settings for MC version + loader.
pub async fn install_zip_vanilla(
    client: &ModrinthClient,
    path: &str,
    mc: &Path,
    root: &Path,
    config: &PackConfig,
) -> Result<String> {
    let file = std::fs::File::open(path).context("open zip")?;
    let mut zip = zip::ZipArchive::new(file)?;
    let name = config.name.clone();
    let instance = root.join(pack::slugify(&name));
    let mods_dir = instance.join("mods");
    std::fs::create_dir_all(&mods_dir)?;

    let entries: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let mut count = 0usize;
    for entry in &entries {
        if entry.ends_with('/') {
            continue;
        }
        let is_jar = entry.to_lowercase().ends_with(".jar");
        let in_mods = entry.starts_with("mods/") || !entry.contains('/');
        if is_jar && in_mods {
            let dest = mods_dir.join(pack::safe_filename(entry));
            let mut src = zip.by_name(entry)?;
            let mut dst = std::fs::File::create(&dest)?;
            std::io::copy(&mut src, &mut dst)?;
            count += 1;
        } else if entry.starts_with("config/") {
            let rel = entry.trim_start_matches("config/");
            if let Some(dest) = safe_dest(&instance.join("config"), rel) {
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut src = zip.by_name(entry)?;
                let mut dst = std::fs::File::create(&dest)?;
                std::io::copy(&mut src, &mut dst)?;
            }
        }
    }
    if config.mc_version.is_empty() {
        anyhow::bail!("set an MC version (Build tab) before loading a .zip");
    }
    let mut log = format!("✓ extracted {count} mod(s)\n");
    let version_id = ensure_loader_version(client, mc, config, &mut log).await?;
    add_launcher_profile(mc, &name, &version_id, &instance)?;
    log.push_str(&format!("✓ profile \"{name}\" ready — press Play"));
    Ok(log)
}

/// Install a .zip into a Prism instance (jars → .minecraft/mods, config →
/// .minecraft/config), then it can be launched from Prism.
pub fn install_zip_prism(path: &str, data_dir: &str, config: &PackConfig) -> Result<String> {
    let file = std::fs::File::open(path).context("open zip")?;
    let mut zip = zip::ZipArchive::new(file)?;
    if config.mc_version.is_empty() {
        anyhow::bail!("set an MC version (Build tab) before loading a .zip into Prism");
    }
    let id = pack::slugify(&config.name);
    let inst = Path::new(data_dir).join("instances").join(&id);
    let gamedir = inst.join(".minecraft");
    std::fs::create_dir_all(gamedir.join("mods"))?;

    let entries: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let mut count = 0usize;
    for entry in &entries {
        if entry.ends_with('/') {
            continue;
        }
        let is_jar = entry.to_lowercase().ends_with(".jar");
        let in_mods = entry.starts_with("mods/") || !entry.contains('/');
        let rel: Option<(PathBuf, String)> = if is_jar && in_mods {
            Some((gamedir.join("mods"), pack::safe_filename(entry)))
        } else if entry.starts_with("config/") {
            Some((
                gamedir.join("config"),
                entry.trim_start_matches("config/").to_string(),
            ))
        } else {
            None
        };
        if let Some((base, rel)) = rel {
            let Some(dest) = safe_dest(&base, &rel) else { continue };
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut src = zip.by_name(entry)?;
            let mut dst = std::fs::File::create(&dest)?;
            std::io::copy(&mut src, &mut dst)?;
            if is_jar {
                count += 1;
            }
        }
    }
    std::fs::write(
        inst.join("instance.cfg"),
        format!("InstanceType=OneSix\nname={}\nicon=default\n", config.name),
    )?;
    std::fs::write(
        inst.join("mmc-pack.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "formatVersion": 1,
            "components": [
                { "uid": "net.minecraft", "version": config.mc_version },
                {
                    "uid": prism_component_uid(&config.loader)?,
                    "version": config.loader_version,
                    "important": true
                }
            ]
        }))?,
    )?;
    Ok(format!(
        "✓ installed {count} mod(s) as Prism instance \"{id}\" — launch it from Prism"
    ))
}

/// Does a pack/instance with this name already exist for this launcher?
pub fn pack_instance_exists(launcher: &LauncherInfo, name: &str) -> bool {
    let slug = pack::slugify(name);
    match launcher.kind.as_str() {
        "prism" => launcher
            .data_dir
            .as_deref()
            .map(|dd| Path::new(dd).join("instances").join(&slug).is_dir())
            .unwrap_or(false),
        "tlauncher" => {
            let dir = launcher
                .data_dir
                .as_deref()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(tlauncher_gamedir);
            dir.map(|mc| {
                let dn = tlauncher_dirname(name);
                mc.join("versions").join(&dn).join(format!("{dn}.json")).is_file()
            })
            .unwrap_or(false)
        }
        _ => {
            let inst_exists = instances_root()
                .map(|r| r.join(&slug).is_dir())
                .unwrap_or(false);
            let prof_exists = minecraft_dir()
                .and_then(|mc| {
                    std::fs::read_to_string(mc.join("launcher_profiles.json")).ok()
                })
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .map(|doc| {
                    doc["profiles"]
                        .get(format!("modpack-manager-{slug}"))
                        .is_some()
                })
                .unwrap_or(false);
            inst_exists || prof_exists
        }
    }
}

/// Peek at a pack file's identity (name, MC version, loader) without installing.
pub fn pack_file_info(path: &str) -> Result<serde_json::Value> {
    use std::io::Read;
    let p = Path::new(path);
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mrpack" => {
            let file = std::fs::File::open(p).context("open mrpack")?;
            let mut zip = zip::ZipArchive::new(file)?;
            let mut text = String::new();
            zip.by_name("modrinth.index.json")
                .context("mrpack missing modrinth.index.json")?
                .read_to_string(&mut text)?;
            let index: serde_json::Value = serde_json::from_str(&text)?;
            let deps = &index["dependencies"];
            let (loader, loader_ver) = [
                ("fabric-loader", "fabric"),
                ("quilt-loader", "quilt"),
                ("forge", "forge"),
                ("neoforge", "neoforge"),
            ]
            .iter()
            .find(|(k, _)| deps.get(*k).is_some())
            .map(|(k, l)| (l.to_string(), deps[*k].as_str().unwrap_or("").to_string()))
            .unwrap_or_default();
            Ok(serde_json::json!({
                "name": index["name"].as_str().unwrap_or("Imported Pack"),
                "version": index["versionId"].as_str().unwrap_or(""),
                "mcVersion": deps["minecraft"].as_str().unwrap_or(""),
                "loader": loader,
                "loaderVersion": loader_ver,
            }))
        }
        "zip" => Ok(serde_json::json!({
            "name": p.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default(),
        })),
        _ => anyhow::bail!("not a pack file: {path}"),
    }
}

/// Route a pack file (.mrpack/.zip) into the chosen launcher.
pub async fn load_pack_file(
    client: &ModrinthClient,
    launcher: &LauncherInfo,
    path: &str,
    config: &PackConfig,
) -> Result<String> {
    let p = Path::new(path);
    if p.is_dir() {
        return load_folder(client, launcher, path, config).await;
    }
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match (launcher.kind.as_str(), ext.as_str()) {
        ("prism", "mrpack") => {
            let dd = launcher
                .data_dir
                .as_deref()
                .context("prism data dir not found")?;
            install_mrpack_prism(path, dd).await
        }
        ("prism", "zip") => {
            let dd = launcher
                .data_dir
                .as_deref()
                .context("prism data dir not found")?;
            install_zip_prism(path, dd, config)
        }
        ("tlauncher", "mrpack") => {
            let mc = tlauncher_mc(&launcher)?;
            install_mrpack_tlauncher(client, path, &mc).await
        }
        ("tlauncher", "zip") => {
            let mc = tlauncher_mc(&launcher)?;
            install_zip_tlauncher(client, path, &mc, config).await
        }
        (_, "mrpack") => {
            let mc = minecraft_dir().context(".minecraft not found")?;
            install_mrpack_vanilla(client, path, &mc, &instances_root()?).await
        }
        (_, "zip") => {
            let mc = minecraft_dir().context(".minecraft not found")?;
            install_zip_vanilla(client, path, &mc, &instances_root()?, config).await
        }
        (_, other) => anyhow::bail!("unsupported file type: .{other}"),
    }
}

/// Mirror jars src→dest (flattened by filename), then remove any dest
/// jars the source no longer has — a pack instance we manage should match
/// the pack, not accumulate jars the host removed. Non-jar files and
/// subdirs in dest are left alone. Returns (copied, removed).
pub fn sync_jars(src_dir: &Path, dest_mods: &Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(dest_mods)?;
    let mut count = 0usize;
    let mut want = std::collections::HashSet::new();
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

/// The single way a "folder of jars + pack meta" becomes a playable
/// launcher entry — synced packs, the host's own folder, and folder loads
/// from the Loader tab all go through here so every path mirrors the same
/// rules (copy jars, prune stale ones, resolve the loader version).
pub async fn deploy_mods_dir(
    client: &ModrinthClient,
    mods_dir: &Path,
    cfg: &PackConfig,
    launcher: &LauncherInfo,
    root: &Path,
) -> Result<String> {
    let mut log = String::new();
    match launcher.kind.as_str() {
        "prism" => {
            let dd = launcher.data_dir.as_deref().context("prism data dir not found")?;
            let id = pack::slugify(&cfg.name);
            let inst = Path::new(dd).join("instances").join(&id);
            let gamedir = inst.join(".minecraft");
            let (n, removed) = sync_jars(mods_dir, &gamedir.join("mods"))?;
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
                        { "uid": prism_component_uid(&cfg.loader)?,
                          "version": cfg.loader_version, "important": true }
                    ]
                }))?,
            )?;
            log.push_str(&format!(
                "✓ wrote Prism instance \"{id}\" — {n} mod(s) ({} {})",
                cfg.loader, cfg.loader_version
            ));
        }
        "tlauncher" => {
            let mc = launcher
                .data_dir
                .as_deref()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(tlauncher_gamedir)
                .context("TLauncher game dir not found")?;
            let gamedir = tlauncher_stage_version(client, &mc, cfg, &mut log).await?;
            let (n, removed) = sync_jars(mods_dir, &gamedir.join("mods"))?;
            log.push_str(&format!("✓ staged {n} mod(s)\n"));
            if removed > 0 {
                log.push_str(&format!("✓ removed {removed} outdated mod(s)\n"));
            }
            log.push_str(&format!(
                "✓ \"{}\" in TLauncher version list — open TLauncher, pick it, press Play",
                cfg.name
            ));
        }
        _ => {
            let mc = minecraft_dir().context(".minecraft not found")?;
            let instance = root.join(pack::slugify(&cfg.name));
            let (n, removed) = sync_jars(mods_dir, &instance.join("mods"))?;
            log.push_str(&format!("✓ staged {n} mod(s)\n"));
            if removed > 0 {
                log.push_str(&format!("✓ removed {removed} outdated mod(s)\n"));
            }
            let version_id = ensure_loader_version(client, &mc, cfg, &mut log).await?;
            add_launcher_profile(&mc, &cfg.name, &version_id, &instance)?;
            log.push_str(&format!(
                "✓ profile \"{}\" ready — open your launcher, pick it, press Play",
                cfg.name
            ));
        }
    }
    Ok(log)
}

/// A plain folder of .jar files, loaded like a pack. Meta comes from the
/// UI's PackConfig; whatever's missing is guessed from the jars
/// themselves (same heuristics the host publish dialog uses).
pub async fn load_folder(
    client: &ModrinthClient,
    launcher: &LauncherInfo,
    folder: &str,
    config: &PackConfig,
) -> Result<String> {
    let dir = Path::new(folder);
    if !dir.is_dir() {
        anyhow::bail!("not a folder: {folder}");
    }
    let mut cfg = config.clone();
    if cfg.name.is_empty() {
        cfg.name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "My Modpack".into());
    }
    if cfg.mc_version.is_empty() || cfg.loader_version.is_empty() {
        let meta = crate::scanner::scan_folder_meta(folder)?;
        let (mc, loader) = crate::sync::suggest_meta(&meta.mods);
        if cfg.mc_version.is_empty() {
            cfg.mc_version = mc;
        }
        if cfg.loader.is_empty() {
            cfg.loader = if loader.is_empty() { "fabric".into() } else { loader };
        }
    }
    if cfg.mc_version.is_empty() {
        anyhow::bail!("couldn't guess an MC version from these jars — set one in Build");
    }
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
    deploy_mods_dir(client, dir, &cfg, launcher, &instances_root()?).await
}

fn tlauncher_mc(launcher: &LauncherInfo) -> Result<PathBuf> {
    launcher
        .data_dir
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(tlauncher_gamedir)
        .context("TLauncher game dir not found")
}

/// Open an existing .mrpack with Prism's importer, or the OS association.
pub fn open_mrpack_file(path: &str) -> Result<String> {
    let p = Path::new(path);
    if !p.is_file() || p.extension().map(|e| e != "mrpack").unwrap_or(true) {
        anyhow::bail!("not an .mrpack file: {path}");
    }
    let detected = detect();
    let prism = detected.launchers.iter().find(|l| l.kind == "prism");
    if let Some(exe) = prism.and_then(|l| l.path.as_deref()) {
        std::process::Command::new(exe)
            .args(["--import", path])
            .spawn()
            .context("run prismlauncher --import")?;
        return Ok("✓ sent to `prismlauncher --import` — finish the import in Prism".into());
    }
    std::process::Command::new("cmd")
        .args(["/c", "start", "", path])
        .spawn()?;
    Ok("✓ opened the .mrpack with its default app".into())
}

/// Deploy to Prism: write a real instance (instance.cfg + mmc-pack.json +
/// .minecraft/mods) into Prism's instances dir, then launch it.
/// Falls back to writing + opening a .mrpack if anything's missing.
pub async fn deploy_prism(
    client: &ModrinthClient,
    mods: &[ModInfo],
    config: &PackConfig,
    exe: Option<&str>,
    data_dir: Option<&str>,
) -> Result<String> {
    // Resolve the loader version if the user left it blank.
    let loader_version = if config.loader_version.is_empty() {
        client
            .loader_versions(&config.loader, &config.mc_version)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default()
    } else {
        config.loader_version.clone()
    };

    // Preferred path: direct instance + --launch.
    if let Some(dd) = data_dir {
        let id = pack::slugify(&config.name);
        let inst = Path::new(dd).join("instances").join(&id);
        let gamedir = inst.join(".minecraft");
        let mut log = String::new();

        let n = pack::stage_instance(&gamedir, mods, config)?;
        log.push_str(&format!(
            "✓ staged {} mod(s) into {}\n",
            n,
            gamedir.to_string_lossy()
        ));

        std::fs::create_dir_all(&inst)?;
        std::fs::write(
            inst.join("instance.cfg"),
            format!("InstanceType=OneSix\nname={}\nicon=default\n", config.name),
        )?;
        std::fs::write(
            inst.join("mmc-pack.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "formatVersion": 1,
                "components": [
                    { "uid": "net.minecraft", "version": config.mc_version },
                    {
                        "uid": prism_component_uid(&config.loader)?,
                        "version": loader_version,
                        "important": true
                    }
                ]
            }))?,
        )?;
        log.push_str(&format!(
            "✓ wrote Prism instance \"{id}\" ({} {}) — open Prism and launch it",
            config.loader, loader_version
        ));
        let _ = exe; // loaded, not launched
        return Ok(log);
    }

    // Fallback: emit a .mrpack and hand it to Prism/file association.
    let (bytes, resolved, overrides) = pack::mrpack_bytes(client, mods, config).await?;
    let dir = instances_root()?.join(pack::slugify(&config.name));
    std::fs::create_dir_all(&dir)?;
    let mrpack_path = dir.join(format!("{}.mrpack", pack::slugify(&config.name)));
    std::fs::write(&mrpack_path, bytes)?;
    let mut log = format!(
        "✓ wrote {} ({} mod refs, {} bundled)\n",
        mrpack_path.to_string_lossy(),
        resolved,
        overrides
    );
    if let Some(exe) = exe {
        let _ = std::process::Command::new(exe)
            .args(["--import", &mrpack_path.to_string_lossy()])
            .spawn();
        log.push_str("✓ sent to `prismlauncher --import` — finish the import in Prism");
    } else {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &mrpack_path.to_string_lossy()])
            .spawn()?;
        log.push_str("✓ opened the .mrpack — import it in your launcher");
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mm-tl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(loader: &str) -> PackConfig {
        PackConfig {
            name: "My Modpack".into(),
            version: "1.0.0".into(),
            summary: String::new(),
            mc_version: "1.20.1".into(),
            loader: loader.into(),
            loader_version: "47.2.0".into(),
            selected_paths: vec![],
        }
    }

    #[test]
    fn dirname_keeps_pretty_name_sanitizes_illegal() {
        assert_eq!(tlauncher_dirname("My Modpack"), "My Modpack");
        assert_eq!(tlauncher_dirname("Star Technology 1.20.1-THETA"), "Star Technology 1.20.1-THETA");
        assert_eq!(tlauncher_dirname(r"a/b\c:d"), "a-b-c-d");
        assert_eq!(tlauncher_dirname("   "), "Modpack");
        assert_eq!(tlauncher_dirname("name..."), "name");
    }

    #[test]
    fn read_prop_unescapes_java_properties() {
        let d = tmpdir("prop");
        let f = d.join("tlauncher-2.0.properties");
        std::fs::write(&f, "minecraft.gamedir=C\\:\\\\Users\\\\ACER\\\\mc\nother=1\n").unwrap();
        assert_eq!(
            read_prop(&f, "minecraft.gamedir").unwrap(),
            "C:\\Users\\ACER\\mc"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn stage_version_writes_self_contained_pack() {
        let d = tmpdir("stage");
        let mc = d.join(".minecraft");
        // Fake an installed forge version for ensure_loader_version to reuse.
        let fv = mc.join("versions").join("1.20.1-forge-47.2.0");
        std::fs::create_dir_all(&fv).unwrap();
        std::fs::write(
            fv.join("1.20.1-forge-47.2.0.json"),
            r#"{"id":"1.20.1-forge-47.2.0","mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher","type":"release"}"#,
        )
        .unwrap();

        let client = crate::modrinth::ModrinthClient::new();
        let mut log = String::new();
        let gamedir = tlauncher_stage_version(&client, &mc, &cfg("forge"), &mut log)
            .await
            .unwrap();

        // versions/My Modpack/ IS the game dir
        assert_eq!(gamedir, mc.join("versions").join("My Modpack"));
        let json_path = gamedir.join("My Modpack.json");
        assert!(json_path.is_file(), "pack version json exists");

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(doc["id"], "My Modpack");
        assert_eq!(doc["jar"], "1.20.1");
        assert_eq!(doc["source"], "local_version_repo");
        assert_eq!(doc["mainClass"], "cpw.mods.bootstraplauncher.BootstrapLauncher");
        assert!(gamedir.join("TLauncherAdditional.json").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn deploy_stages_mods_inside_version_dir() {
        let d = tmpdir("deploy");
        let mc = d.join(".minecraft");
        let fv = mc.join("versions").join("1.20.1-forge-47.2.0");
        std::fs::create_dir_all(&fv).unwrap();
        std::fs::write(
            fv.join("1.20.1-forge-47.2.0.json"),
            r#"{"id":"1.20.1-forge-47.2.0","mainClass":"x","type":"release"}"#,
        )
        .unwrap();

        // A mod jar to stage.
        let jar = d.join("coolmod.jar");
        std::fs::write(&jar, b"fake jar bytes").unwrap();
        let mods = vec![ModInfo {
            path: jar.to_string_lossy().into(),
            filename: "coolmod.jar".into(),
            size: 14,
            sha1: String::new(),
            sha512: String::new(),
            mod_id: None,
            name: None,
            version: None,
            loaders: vec![],
            environment: None,
            game_versions: vec![],
            parse_error: None,
        }];

        let client = crate::modrinth::ModrinthClient::new();
        let mut config = cfg("forge");
        config.selected_paths = vec![jar.to_string_lossy().into_owned()];
        let out = deploy_tlauncher(
            &client,
            &mods,
            &config,
            Some(mc.to_str().unwrap()),
        )
        .await
        .unwrap();

        let gamedir = mc.join("versions").join("My Modpack");
        assert!(gamedir.join("mods").join("coolmod.jar").is_file(),
            "mod jar lands INSIDE the version folder (it is the game dir)");
        assert!(gamedir.join("My Modpack.json").is_file());
        assert!(out.contains("press Play"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
