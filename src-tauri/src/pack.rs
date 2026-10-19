use crate::modrinth::{ModrinthClient, ResolvedMod};
use crate::scanner::ModInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackConfig {
    pub name: String,
    pub version: String,
    pub summary: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "loaderVersion")]
    pub loader_version: String,
    #[serde(rename = "selectedPaths")]
    pub selected_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildResult {
    #[serde(rename = "outputPath")]
    pub output_path: String,
    #[serde(rename = "resolvedCount")]
    pub resolved_count: usize,
    #[serde(rename = "overrideCount")]
    pub override_count: usize,
}

fn selected<'a>(mods: &'a [ModInfo], config: &PackConfig) -> Vec<&'a ModInfo> {
    let want: std::collections::HashSet<&str> =
        config.selected_paths.iter().map(|s| s.as_str()).collect();
    mods.iter().filter(|m| want.contains(m.path.as_str())).collect()
}

pub fn safe_filename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "mod.jar".into())
        .replace(['/', '\\'], "_")
}

pub fn slugify(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut out = String::new();
    for c in s.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() { "pack".into() } else { trimmed }
}

/// Look up the currently-selected mods on Modrinth, keyed by sha1.
pub async fn resolve_selection(
    client: &ModrinthClient,
    mods: &[ModInfo],
) -> HashMap<String, ResolvedMod> {
    let hashes: Vec<String> = mods
        .iter()
        .map(|m| m.sha1.clone())
        .filter(|h| !h.is_empty())
        .collect();
    client.resolve_hashes(&hashes).await.unwrap_or_default()
}

/// Build the modrinth.index.json value.
pub fn mrpack_index(
    config: &PackConfig,
    selected: &[&ModInfo],
    resolved: &HashMap<String, ResolvedMod>,
) -> serde_json::Value {
    let mut files = vec![];
    for m in selected {
        if let Some(r) = resolved.get(&m.sha1) {
            let Some(url) = r.download_url.clone() else { continue };
            let (client_env, server_env) = env_for(r);
            files.push(serde_json::json!({
                "path": format!("mods/{}", r.filename.clone().unwrap_or_else(|| safe_filename(&m.filename))),
                "hashes": { "sha1": m.sha1, "sha512": m.sha512 },
                "downloads": [url],
                "env": { "client": client_env, "server": server_env },
                "fileSize": m.size,
            }));
        }
    }
    let loader_key = match config.loader.as_str() {
        "fabric" => "fabric-loader",
        "quilt" => "quilt-loader",
        "forge" => "forge",
        "neoforge" => "neoforge",
        other => other,
    };
    let mut deps = serde_json::json!({ "minecraft": config.mc_version });
    if !config.loader_version.is_empty() {
        deps[loader_key] = serde_json::Value::String(config.loader_version.clone());
    }
    serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": config.version,
        "name": config.name,
        "summary": config.summary,
        "files": files,
        "dependencies": deps,
    })
}

fn env_for(r: &ResolvedMod) -> (&'static str, &'static str) {
    let map = |s: &Option<String>| match s.as_deref() {
        Some("required") => "required",
        Some("unsupported") => "unsupported",
        _ => "optional",
    };
    (map(&r.client_side), map(&r.server_side))
}

/// Render a .mrpack into memory.
pub async fn mrpack_bytes(
    client: &ModrinthClient,
    all_mods: &[ModInfo],
    config: &PackConfig,
) -> Result<(Vec<u8>, usize, usize)> {
    let sel = selected(all_mods, config);
    let sel_owned: Vec<ModInfo> = sel.iter().map(|m| (*m).clone()).collect();
    let resolved = resolve_selection(client, &sel_owned).await;
    let index = mrpack_index(config, &sel, &resolved);

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("modrinth.index.json", opts)?;
        zip.write_all(serde_json::to_string_pretty(&index)?.as_bytes())?;

        let mut override_count = 0usize;
        for m in &sel {
            if !resolved.contains_key(&m.sha1) {
                let dest = format!("overrides/mods/{}", safe_filename(&m.filename));
                zip.start_file(dest, opts)?;
                let mut f = std::fs::File::open(&m.path)
                    .with_context(|| format!("open {}", m.path))?;
                std::io::copy(&mut f, &mut zip)?;
                override_count += 1;
            }
        }
        zip.finish()?;
        return Ok((buf.into_inner(), index["files"].as_array().map(|a| a.len()).unwrap_or(0), override_count));
    }
}

pub async fn build_mrpack(
    client: &ModrinthClient,
    all_mods: &[ModInfo],
    config: &PackConfig,
    out_dir: &str,
) -> Result<BuildResult> {
    let (bytes, resolved_count, override_count) = mrpack_bytes(client, all_mods, config).await?;
    let path = PathBuf::from(out_dir).join(format!("{}.mrpack", slugify(&config.name)));
    std::fs::write(&path, bytes)?;
    Ok(BuildResult {
        output_path: path.to_string_lossy().to_string(),
        resolved_count,
        override_count,
    })
}

pub fn build_zip(
    all_mods: &[ModInfo],
    config: &PackConfig,
    out_dir: &str,
) -> Result<BuildResult> {
    let sel = selected(all_mods, config);
    let path = PathBuf::from(out_dir).join(format!("{}.zip", slugify(&config.name)));
    let f = std::fs::File::create(&path)?;
    let mut zip = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for m in &sel {
        zip.start_file(format!("mods/{}", safe_filename(&m.filename)), opts)?;
        let mut src = std::fs::File::open(&m.path)?;
        std::io::copy(&mut src, &mut zip)?;
    }
    zip.finish()?;
    Ok(BuildResult {
        output_path: path.to_string_lossy().to_string(),
        resolved_count: 0,
        override_count: sel.len(),
    })
}

pub fn build_modlist(
    all_mods: &[ModInfo],
    resolved: &HashMap<String, ResolvedMod>,
    config: &PackConfig,
    out_dir: &str,
) -> Result<BuildResult> {
    let sel = selected(all_mods, config);
    let path = PathBuf::from(out_dir).join("modlist.txt");
    let mut out = format!(
        "{} {} — Minecraft {} ({})\n\n",
        config.name, config.version, config.mc_version, config.loader
    );
    for m in &sel {
        let r = resolved.get(&m.sha1);
        let name = r
            .and_then(|r| r.project_title.clone())
            .or_else(|| m.name.clone())
            .or_else(|| m.mod_id.clone())
            .unwrap_or_else(|| m.filename.clone());
        let ver = r
            .and_then(|r| r.version_number.clone())
            .or_else(|| m.version.clone())
            .unwrap_or_default();
        out.push_str(&format!("- {name} {ver}  ({}, {})\n", m.filename, human_size(m.size)));
    }
    std::fs::write(&path, out)?;
    Ok(BuildResult {
        output_path: path.to_string_lossy().to_string(),
        resolved_count: resolved.len(),
        override_count: 0,
    })
}

/// Copy selected jars into `<instance_dir>/mods/`.
pub fn stage_instance(instance_dir: &Path, all_mods: &[ModInfo], config: &PackConfig) -> Result<usize> {
    let sel = selected(all_mods, config);
    let mods_dir = instance_dir.join("mods");
    std::fs::create_dir_all(&mods_dir)?;
    // Clean out stale jars from a previous staging of this instance.
    for entry in std::fs::read_dir(&mods_dir)? {
        let entry = entry?;
        if entry.path().extension().map(|e| e == "jar") == Some(true) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut count = 0;
    for m in &sel {
        let dest = mods_dir.join(safe_filename(&m.filename));
        if let Err(_) = std::fs::hard_link(&m.path, &dest) {
            std::fs::copy(&m.path, &dest)?;
        }
        count += 1;
    }
    Ok(count)
}

fn human_size(n: u64) -> String {
    if n > 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", n / (1 << 10))
    }
}

/// Read entire file into memory (used to embed small override files).
#[allow(dead_code)]
pub fn read_all(path: &Path) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_shape() {
        let config = PackConfig {
            name: "Test Pack".into(),
            version: "1.0.0".into(),
            summary: "hi".into(),
            mc_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            selected_paths: vec!["/tmp/a.jar".into()],
        };
        let m = ModInfo {
            path: "/tmp/a.jar".into(),
            filename: "a.jar".into(),
            size: 10,
            sha1: "abc".into(),
            sha512: "def".into(),
            mod_id: None,
            name: None,
            version: None,
            loaders: vec![],
            environment: None,
            game_versions: vec![],
            parse_error: None,
        };
        let mut resolved = HashMap::new();
        resolved.insert(
            "abc".into(),
            ResolvedMod {
                sha1: "abc".into(),
                project_id: Some("P".into()),
                version_id: Some("V".into()),
                project_title: Some("T".into()),
                project_slug: Some("t".into()),
                version_number: Some("1.0".into()),
                game_versions: vec!["1.21.1".into()],
                loaders: vec!["fabric".into()],
                download_url: Some("https://cdn.modrinth.com/x.jar".into()),
                file_size: Some(10),
                filename: Some("a.jar".into()),
                client_side: Some("required".into()),
                server_side: Some("optional".into()),
            },
        );
        let sel: Vec<&ModInfo> = vec![&m];
        let idx = mrpack_index(&config, &sel, &resolved);
        assert_eq!(idx["formatVersion"], 1);
        assert_eq!(idx["dependencies"]["minecraft"], "1.21.1");
        assert_eq!(idx["dependencies"]["fabric-loader"], "0.16.9");
        let f = &idx["files"][0];
        assert_eq!(f["path"], "mods/a.jar");
        assert_eq!(f["hashes"]["sha1"], "abc");
        assert_eq!(f["env"]["client"], "required");
    }

    #[test]
    fn slug() {
        assert_eq!(slugify("My Cool Pack!!"), "my-cool-pack");
        assert_eq!(slugify("  "), "pack");
    }

    #[test]
    fn filename_safety() {
        assert_eq!(safe_filename("C:\\a\\b.jar"), "b.jar");
        assert_eq!(safe_filename("../evil.jar"), "evil.jar");
    }
}
