use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModInfo {
    pub path: String,
    pub filename: String,
    pub size: u64,
    pub sha1: String,
    pub sha512: String,
    #[serde(rename = "modId")]
    pub mod_id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    /// Loaders declared by the jar's own metadata (may be empty).
    pub loaders: Vec<String>,
    /// client | server | both
    pub environment: Option<String>,
    /// MC versions declared in jar metadata (usually ranges/empty).
    #[serde(rename = "gameVersions")]
    pub game_versions: Vec<String>,
    #[serde(rename = "parseError")]
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub mods: Vec<ModInfo>,
    #[serde(rename = "totalSize")]
    pub total_size: u64,
}

pub fn scan_folder(path: &str) -> Result<ScanResult> {
    scan_folder_progress(path, |_, _| {})
}

/// Scan `path` recursively for jars, hashing + parsing them in parallel.
/// `on_progress(done, total)` fires after each jar completes.
pub fn scan_folder_progress<F>(path: &str, on_progress: F) -> Result<ScanResult>
where
    F: Fn(usize, usize) + Sync,
{
    let root = Path::new(path);
    if !root.is_dir() {
        anyhow::bail!("not a directory: {path}");
    }
    let mut jars: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension() {
                if ext.eq_ignore_ascii_case("jar") {
                    jars.push(entry.path().to_path_buf());
                }
            }
        }
    }
    scan_paths(jars, on_progress)
}

/// Scan an explicit list of jar paths (picked individually by the user).
pub fn scan_paths<F>(paths: Vec<PathBuf>, on_progress: F) -> Result<ScanResult>
where
    F: Fn(usize, usize) + Sync,
{
    let mut jars: Vec<PathBuf> = paths
        .into_iter()
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .map(|e| e.eq_ignore_ascii_case("jar"))
                    .unwrap_or(false)
        })
        .collect();
    jars.sort();
    jars.dedup();
    let total_jars = jars.len();
    let done = AtomicUsize::new(0);

    let mut mods: Vec<ModInfo> = jars
        .par_iter()
        .map(|jar| {
            let m = match inspect_jar(jar) {
                Ok(m) => m,
                Err(e) => ModInfo {
                    path: jar.to_string_lossy().to_string(),
                    filename: jar
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    size: std::fs::metadata(jar).map(|m| m.len()).unwrap_or(0),
                    sha1: String::new(),
                    sha512: String::new(),
                    mod_id: None,
                    name: None,
                    version: None,
                    loaders: vec![],
                    environment: None,
                    game_versions: vec![],
                    parse_error: Some(format!("{e:#}")),
                },
            };
            on_progress(done.fetch_add(1, Ordering::Relaxed) + 1, total_jars);
            m
        })
        .collect();

    let total = mods.iter().map(|m| m.size).sum();
    mods.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(ScanResult { mods, total_size: total })
}

fn inspect_jar(path: &Path) -> Result<ModInfo> {
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    // Single open: stream the whole file once through both hashers,
    // then rewind and hand the same handle to the zip reader.
    let mut file = BufReader::new(File::open(path).context("open jar")?);
    let (sha1_hex, sha512_hex, size) = hash_reader(&mut file)?;
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut info = ModInfo {
        path: path.to_string_lossy().to_string(),
        filename,
        size,
        sha1: sha1_hex,
        sha512: sha512_hex,
        mod_id: None,
        name: None,
        version: None,
        loaders: vec![],
        environment: None,
        game_versions: vec![],
        parse_error: None,
    };

    // A jar that isn't a readable zip still carries its content hashes —
    // Connect manifests rely on them; discarding them would make the file
    // undownloadable (members verify sha1 of the served bytes).
    let mut zip = match zip::ZipArchive::new(file) {
        Ok(z) => z,
        Err(e) => {
            info.parse_error = Some(format!("{e:#}"));
            return Ok(info);
        }
    };

    // Central-directory listing only — no per-entry seeks.
    let names: Vec<String> = zip.file_names().map(|n| n.to_string()).collect();

    if names.iter().any(|n| n == "fabric.mod.json") {
        if let Ok(text) = read_entry(&mut zip, "fabric.mod.json") {
            parse_fabric(&text, &mut info);
        }
    }
    if names.iter().any(|n| n == "quilt.mod.json") {
        if let Ok(text) = read_entry(&mut zip, "quilt.mod.json") {
            parse_quilt(&text, &mut info);
        }
    }
    for (entry, loader) in [
        ("META-INF/neoforge.mods.toml", "neoforge"),
        ("META-INF/mods.toml", "forge"),
    ] {
        if names.iter().any(|n| n == entry) {
            if let Ok(text) = read_entry(&mut zip, entry) {
                parse_forge_toml(&text, loader, &mut info);
            }
        }
    }
    if info.mod_id.is_none() && names.iter().any(|n| n == "mcmod.info") {
        if let Ok(text) = read_entry(&mut zip, "mcmod.info") {
            parse_mcmod_info(&text, &mut info);
        }
    }
    Ok(info)
}

fn hash_reader<R: Read>(f: &mut R) -> Result<(String, String, u64)> {
    let mut s1 = sha1::Sha1::new();
    let mut s5 = sha2::Sha512::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        s1.update(&buf[..n]);
        s5.update(&buf[..n]);
        total += n as u64;
    }
    Ok((
        hex::encode(s1.finalize()),
        hex::encode(s5.finalize()),
        total,
    ))
}

fn read_entry(zip: &mut zip::ZipArchive<BufReader<File>>, name: &str) -> Result<String> {
    let mut f = zip.by_name(name)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

fn parse_fabric(text: &str, info: &mut ModInfo) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        info.parse_error = Some("invalid fabric.mod.json".into());
        return;
    };
    info.loaders.push("fabric".into());
    info.mod_id = v.get("id").and_then(|x| x.as_str()).map(String::from);
    info.name = v.get("name").and_then(|x| x.as_str()).map(String::from);
    info.version = v
        .get("version")
        .and_then(|x| x.as_str())
        .map(String::from);
    info.environment = v
        .get("environment")
        .and_then(|x| x.as_str())
        .map(|e| match e {
            "*" => "both".to_string(),
            other => other.to_string(),
        });
    if let Some(mc) = v
        .get("depends")
        .and_then(|d| d.get("minecraft"))
        .and_then(|m| m.as_str())
    {
        info.game_versions = vec![mc.to_string()];
    }
}

fn parse_quilt(text: &str, info: &mut ModInfo) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        info.parse_error = Some("invalid quilt.mod.json".into());
        return;
    };
    info.loaders.push("quilt".into());
    if let Some(q) = v.get("quilt_loader") {
        info.mod_id = q.get("id").and_then(|x| x.as_str()).map(String::from);
        info.version = q
            .get("version")
            .and_then(|x| x.as_str())
            .map(String::from);
        if let Some(meta) = q.get("metadata") {
            info.name = meta.get("name").and_then(|x| x.as_str()).map(String::from);
        }
    }
    if let Some(mc) = v.get("minecraft").and_then(|m| m.get("environment")) {
        info.environment = mc.as_str().map(String::from);
    }
}

fn parse_forge_toml(text: &str, loader: &str, info: &mut ModInfo) {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        info.parse_error = Some(format!("invalid {loader} mods.toml"));
        return;
    };
    if !info.loaders.contains(&loader.to_string()) {
        info.loaders.push(loader.into());
    }
    if let Some(mods) = v.get("mods").and_then(|m| m.as_array()).and_then(|a| a.first()) {
        if info.mod_id.is_none() {
            info.mod_id = mods.get("modId").and_then(|x| x.as_str()).map(String::from);
        }
        if info.name.is_none() {
            info.name = mods
                .get("displayName")
                .and_then(|x| x.as_str())
                .map(String::from);
        }
        if info.version.is_none() {
            info.version = mods
                .get("version")
                .and_then(|x| x.as_str())
                .filter(|s| !s.contains("${"))
                .map(String::from);
        }
    }
    // [[dependencies.<modid>]] with modId="minecraft" carries the MC range.
    if let Some(range) = v
        .get("dependencies")
        .and_then(|d| d.as_table())
        .and_then(|t| {
            t.values()
                .filter_map(|v| v.as_array())
                .flatten()
                .find(|dep| dep.get("modId").and_then(|m| m.as_str()) == Some("minecraft"))
        })
        .and_then(|dep| dep.get("versionRange"))
        .and_then(|r| r.as_str())
    {
        info.game_versions = vec![range.to_string()];
    }
}

fn parse_mcmod_info(text: &str, info: &mut ModInfo) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    info.loaders.push("forge".into());
    let entry = if v.is_array() {
        v.as_array().and_then(|a| a.first()).cloned()
    } else {
        v.get("modList").and_then(|m| m.as_array()).and_then(|a| a.first()).cloned()
    };
    if let Some(e) = entry {
        info.mod_id = e.get("modid").and_then(|x| x.as_str()).map(String::from);
        info.name = e.get("name").and_then(|x| x.as_str()).map(String::from);
        info.version = e.get("version").and_then(|x| x.as_str()).map(String::from);
        if let Some(mc) = e.get("mcversion").and_then(|x| x.as_str()) {
            info.game_versions = vec![mc.to_string()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn blank() -> ModInfo {
        ModInfo {
            path: "".into(),
            filename: "x.jar".into(),
            size: 0,
            sha1: "".into(),
            sha512: "".into(),
            mod_id: None,
            name: None,
            version: None,
            loaders: vec![],
            environment: None,
            game_versions: vec![],
            parse_error: None,
        }
    }

    #[test]
    fn fabric_parse() {
        let mut info = blank();
        parse_fabric(
            r#"{"id":"sodium","name":"Sodium","version":"0.6.0","environment":"*","depends":{"minecraft":"~1.21"}}"#,
            &mut info,
        );
        assert_eq!(info.mod_id.as_deref(), Some("sodium"));
        assert_eq!(info.loaders, vec!["fabric"]);
        assert_eq!(info.environment.as_deref(), Some("both"));
        assert_eq!(info.game_versions, vec!["~1.21"]);
    }

    #[test]
    fn forge_toml_parse() {
        let mut info = blank();
        parse_forge_toml(
            r#"
modLoader="javafml"
loaderVersion="[47,)"
[[mods]]
modId="jei"
displayName="Just Enough Items"
version="${file.jarVersion}"
[[dependencies.jei]]
modId="minecraft"
versionRange="[1.20.1,1.21)"
"#,
            "neoforge",
            &mut info,
        );
        assert_eq!(info.mod_id.as_deref(), Some("jei"));
        assert_eq!(info.name.as_deref(), Some("Just Enough Items"));
        // placeholder version filtered out
        assert!(info.version.is_none());
        assert_eq!(info.loaders, vec!["neoforge"]);
    }

    #[test]
    fn scan_real_jar() {
        let dir = std::env::temp_dir().join(format!("mcmodder-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("sodium-fake.jar");
        {
            let f = std::fs::File::create(&jar).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("fabric.mod.json", opts).unwrap();
            z.write_all(
                br#"{"id":"sodium","name":"Sodium","version":"0.6.0","environment":"*"}"#,
            )
            .unwrap();
            z.finish().unwrap();
        }
        let res = scan_folder(dir.to_str().unwrap()).unwrap();
        assert_eq!(res.mods.len(), 1);
        let m = &res.mods[0];
        assert_eq!(m.mod_id.as_deref(), Some("sodium"));
        assert_eq!(m.loaders, vec!["fabric"]);
        assert_eq!(m.sha1.len(), 40);
        assert_eq!(m.sha512.len(), 128);
        std::fs::remove_dir_all(&dir).ok();
    }
}
