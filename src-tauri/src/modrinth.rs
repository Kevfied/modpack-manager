use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

const API: &str = "https://api.modrinth.com/v2";
const UA: &str = "mc-modder/0.1.0 (github.com/local)";

#[derive(Clone)]
pub struct ModrinthClient {
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedMod {
    pub sha1: String,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    #[serde(rename = "versionId")]
    pub version_id: Option<String>,
    #[serde(rename = "projectTitle")]
    pub project_title: Option<String>,
    #[serde(rename = "projectSlug")]
    pub project_slug: Option<String>,
    #[serde(rename = "versionNumber")]
    pub version_number: Option<String>,
    #[serde(rename = "gameVersions")]
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    #[serde(rename = "downloadUrl")]
    pub download_url: Option<String>,
    #[serde(rename = "fileSize")]
    pub file_size: Option<u64>,
    pub filename: Option<String>,
    #[serde(rename = "clientSide")]
    pub client_side: Option<String>,
    #[serde(rename = "serverSide")]
    pub server_side: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyWarning {
    #[serde(rename = "modName")]
    pub mod_name: String,
    #[serde(rename = "requiredProject")]
    pub required_project: String,
    #[serde(rename = "requiredTitle")]
    pub required_title: Option<String>,
}

impl ModrinthClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self { http }
    }

    /// Raw HTTP client, for non-Modrinth endpoints (sync peers etc.).
    pub fn inner_http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Batch-look up jars by SHA-1. Returns map sha1 -> ResolvedMod.
    pub async fn resolve_hashes(&self, hashes: &[String]) -> Result<HashMap<String, ResolvedMod>> {
        let mut out = HashMap::new();
        let futs: Vec<_> = hashes
            .chunks(200)
            .map(|chunk| {
                let http = self.http.clone();
                let chunk = chunk.to_vec();
                async move {
                    http.post(format!("{API}/version_files"))
                        .json(&serde_json::json!({
                            "hashes": chunk,
                            "algorithm": "sha1"
                        }))
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<HashMap<String, serde_json::Value>>()
                        .await
                }
            })
            .collect();
        for resp in futures::future::join_all(futs).await {
            for (hash, version) in resp? {
                out.insert(hash, self.version_to_resolved(&version));
            }
        }
        // Enrich with project metadata (title, side support).
        let ids: Vec<String> = out
            .values()
            .filter_map(|r| r.project_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if !ids.is_empty() {
            let ids_json = serde_json::to_string(&ids)?;
            let projects: Vec<serde_json::Value> = self
                .http
                .get(format!("{API}/projects"))
                .query(&[("ids", ids_json)])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let mut pmap: HashMap<String, &serde_json::Value> = HashMap::new();
            for p in &projects {
                if let Some(id) = p.get("id").and_then(|i| i.as_str()) {
                    pmap.insert(id.to_string(), p);
                }
            }
            for r in out.values_mut() {
                if let Some(pid) = &r.project_id {
                    if let Some(p) = pmap.get(pid) {
                        r.project_title =
                            p.get("title").and_then(|t| t.as_str()).map(String::from);
                        r.project_slug =
                            p.get("slug").and_then(|t| t.as_str()).map(String::from);
                        r.client_side =
                            p.get("client_side").and_then(|t| t.as_str()).map(String::from);
                        r.server_side =
                            p.get("server_side").and_then(|t| t.as_str()).map(String::from);
                    }
                }
            }
        }
        Ok(out)
    }

    fn version_to_resolved(&self, v: &serde_json::Value) -> ResolvedMod {
        let primary = v
            .get("files")
            .and_then(|f| f.as_array())
            .and_then(|files| {
                files
                    .iter()
                    .find(|f| f.get("primary").and_then(|p| p.as_bool()) == Some(true))
                    .or_else(|| files.first())
            });
        ResolvedMod {
            sha1: primary
                .and_then(|f| {
                    f.get("hashes")
                        .and_then(|h| h.get("sha1"))
                        .and_then(|s| s.as_str())
                })
                .unwrap_or_default()
                .to_string(),
            project_id: v.get("project_id").and_then(|s| s.as_str()).map(String::from),
            version_id: v.get("id").and_then(|s| s.as_str()).map(String::from),
            project_title: None,
            project_slug: None,
            version_number: v
                .get("version_number")
                .and_then(|s| s.as_str())
                .map(String::from),
            game_versions: v
                .get("game_versions")
                .and_then(|g| g.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            loaders: v
                .get("loaders")
                .and_then(|g| g.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            download_url: primary
                .and_then(|f| f.get("url"))
                .and_then(|s| s.as_str())
                .map(String::from),
            file_size: primary
                .and_then(|f| f.get("size"))
                .and_then(|s| s.as_u64()),
            filename: primary
                .and_then(|f| f.get("filename"))
                .and_then(|s| s.as_str())
                .map(String::from),
            client_side: None,
            server_side: None,
        }
    }

    /// Release-type MC versions, newest first.
    pub async fn game_versions(&self) -> Result<Vec<String>> {
        let tags: Vec<serde_json::Value> = self
            .http
            .get(format!("{API}/tag/game_version"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tags
            .iter()
            .filter(|t| t.get("version_type").and_then(|v| v.as_str()) == Some("release"))
            .filter_map(|t| t.get("version").and_then(|v| v.as_str()).map(String::from))
            .collect())
    }

    /// Loader versions for a given MC version, best first.
    pub async fn loader_versions(&self, loader: &str, mc: &str) -> Result<Vec<String>> {
        match loader {
            "fabric" => self.fabric_loader_versions(mc).await,
            "quilt" => self.quilt_loader_versions().await,
            "forge" => self.forge_loader_versions(mc).await,
            "neoforge" => self.neoforge_loader_versions(mc).await,
            _ => Ok(vec![]),
        }
    }

    async fn fabric_loader_versions(&self, mc: &str) -> Result<Vec<String>> {
        let rows: Vec<serde_json::Value> = self
            .http
            .get(format!("https://meta.fabricmc.net/v2/versions/loader/{mc}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut stable = vec![];
        let mut all = vec![];
        for r in &rows {
            if let Some(l) = r.get("loader") {
                let v = l.get("version").and_then(|x| x.as_str()).unwrap_or_default();
                all.push(v.to_string());
                if l.get("stable").and_then(|s| s.as_bool()) == Some(true) {
                    stable.push(v.to_string());
                }
            }
        }
        Ok(if stable.is_empty() { all } else { stable })
    }

    async fn quilt_loader_versions(&self) -> Result<Vec<String>> {
        let rows: Vec<serde_json::Value> = self
            .http
            .get("https://meta.quiltmc.org/v3/versions/loader")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get("version").and_then(|v| v.as_str()).map(String::from))
            .take(10)
            .collect())
    }

    async fn forge_loader_versions(&self, mc: &str) -> Result<Vec<String>> {
        let promos: serde_json::Value = self
            .http
            .get("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut out = vec![];
        if let Some(p) = promos.get("promos").and_then(|p| p.as_object()) {
            for key in [format!("{mc}-recommended"), format!("{mc}-latest")] {
                if let Some(v) = p.get(&key).and_then(|v| v.as_str()) {
                    if !out.contains(&v.to_string()) {
                        out.push(v.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    async fn neoforge_loader_versions(&self, mc: &str) -> Result<Vec<String>> {
        // NeoForge version scheme: MC 1.20.1 -> 20.1.x ; MC 1.X.Y (X>=21) -> X.Y.x
        let parts: Vec<&str> = mc.split('.').collect();
        let prefix = match parts.as_slice() {
            ["1", minor, patch] => format!("{minor}.{patch}."),
            ["1", minor] => format!("{minor}."),
            _ => return Ok(vec![]),
        };
        let xml = self
            .http
            .get("https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let mut versions: Vec<String> = xml
            .split("<version>")
            .skip(1)
            .filter_map(|s| s.split("</version>").next().map(String::from))
            .filter(|v| v.starts_with(&prefix) && !v.contains("beta") && !v.contains("alpha"))
            .collect();
        versions.reverse(); // metadata is oldest-first
        versions.truncate(5);
        Ok(versions)
    }

    /// Required dependencies of a Modrinth version. Returns project_ids of deps
    /// of type "required" (and "embedded" resolved separately — ignored).
    pub async fn required_deps(&self, version_id: &str) -> Result<Vec<String>> {
        let v: serde_json::Value = self
            .http
            .get(format!("{API}/version/{version_id}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut out = vec![];
        if let Some(deps) = v.get("dependencies").and_then(|d| d.as_array()) {
            for d in deps {
                let is_required = d.get("dependency_type").and_then(|t| t.as_str()) == Some("required");
                let pid = d.get("project_id").and_then(|p| p.as_str());
                if is_required {
                    if let Some(pid) = pid {
                        out.push(pid.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn project_title(&self, project_id: &str) -> Result<Option<String>> {
        let p: serde_json::Value = self
            .http
            .get(format!("{API}/project/{project_id}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(p.get("title").and_then(|t| t.as_str()).map(String::from))
    }

    /// Given jar hashes + a target loader/MC, return newer compatible versions.
    /// Map sha1 -> ResolvedMod (only for hashes that have a compatible build).
    pub async fn updates_for(
        &self,
        hashes: &[String],
        loader: &str,
        mc_version: &str,
    ) -> Result<HashMap<String, ResolvedMod>> {
        let resp: HashMap<String, serde_json::Value> = self
            .http
            .post(format!("{API}/version_files/update"))
            .json(&serde_json::json!({
                "hashes": hashes,
                "algorithm": "sha1",
                "loaders": [loader],
                "game_versions": [mc_version]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut out = HashMap::new();
        for (hash, version) in resp {
            let mut r = self.version_to_resolved(&version);
            r.sha1 = hash.clone();
            out.insert(hash, r);
        }
        Ok(out)
    }

    /// Fetch a Fabric loader profile JSON ready to drop into versions/.
    pub async fn fabric_profile(&self, mc: &str, loader_version: &str) -> Result<serde_json::Value> {
        let url = format!(
            "https://meta.fabricmc.net/v2/versions/loader/{mc}/{loader_version}/profile/json"
        );
        let v: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("fetch fabric profile {url}"))?
            .json()
            .await?;
        Ok(v)
    }

    /// Same for Quilt.
    pub async fn quilt_profile(&self, mc: &str, loader_version: &str) -> Result<serde_json::Value> {
        let url = format!(
            "https://meta.quiltmc.org/v3/versions/loader/{mc}/{loader_version}/profile/json"
        );
        let v: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("fetch quilt profile {url}"))?
            .json()
            .await?;
        Ok(v)
    }
}
