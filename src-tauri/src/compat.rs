use crate::modrinth::ResolvedMod;
use crate::scanner::ModInfo;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    #[serde(rename = "suggestedMcVersion")]
    pub suggested_mc_version: Option<String>,
    #[serde(rename = "suggestedLoader")]
    pub suggested_loader: Option<String>,
    #[serde(rename = "commonGameVersions")]
    pub common_game_versions: Vec<String>,
    #[serde(rename = "commonLoaders")]
    pub common_loaders: Vec<String>,
    pub conflicts: Vec<String>,
    #[serde(rename = "missingDeps")]
    pub missing_deps: Vec<String>,
}

/// Intersect loaders and MC versions across the selected mods, preferring
/// Modrinth-derived data and falling back to jar-declared metadata.
pub fn analyze(
    mods: &[ModInfo],
    resolved: &HashMap<String, ResolvedMod>,
    selected: &HashSet<String>,
) -> CompatReport {
    let chosen: Vec<&ModInfo> = mods
        .iter()
        .filter(|m| selected.contains(&m.path))
        .collect();

    let mut loader_sets: Vec<(String, HashSet<String>)> = vec![];
    let mut version_sets: Vec<(String, HashSet<String>)> = vec![];
    let mut loader_votes: HashMap<String, usize> = HashMap::new();

    for m in &chosen {
        let label = m.filename.clone();
        let r = resolved.get(&m.sha1);

        let mut loaders: HashSet<String> = r
            .map(|r| r.loaders.iter().cloned().collect())
            .unwrap_or_default();
        if loaders.is_empty() {
            loaders = m.loaders.iter().cloned().collect();
        }
        // Normalize aliases.
        let loaders: HashSet<String> = loaders
            .into_iter()
            .map(|l| match l.as_str() {
                "fabric-loader" | "fabricmc" => "fabric".into(),
                "quilt-loader" => "quilt".into(),
                other => other.to_string(),
            })
            .collect();
        for l in &loaders {
            *loader_votes.entry(l.clone()).or_default() += 1;
        }
        if !loaders.is_empty() {
            loader_sets.push((label.clone(), loaders));
        }

        let mut gvs: HashSet<String> = r
            .map(|r| r.game_versions.iter().cloned().collect())
            .unwrap_or_default();
        if gvs.is_empty() {
            gvs = m.game_versions.iter().cloned().collect();
        }
        if !gvs.is_empty() {
            version_sets.push((label.clone(), gvs));
        }
    }

    // Common loaders = loaders every mod (that declares any) supports.
    let known_loaders = ["fabric", "forge", "neoforge", "quilt"];
    let common_loaders: Vec<String> = if loader_sets.is_empty() {
        vec![]
    } else {
        known_loaders
            .iter()
            .filter(|l| loader_sets.iter().all(|(_, s)| s.contains(**l)))
            .map(|l| l.to_string())
            .collect()
    };

    // Common game versions across mods that publish version lists.
    let mut common_gv: Vec<String> = vec![];
    if let Some((_, first)) = version_sets.first() {
        for v in first {
            if version_sets.iter().all(|(_, s)| s.contains(v)) {
                common_gv.push(v.clone());
            }
        }
    }
    common_gv.sort_by(|a, b| version_key(b).cmp(&version_key(a)));

    let mut conflicts = vec![];
    for (label, loaders) in &loader_sets {
        if !common_loaders.is_empty() && loaders.is_disjoint(&common_loaders.iter().cloned().collect()) {
            conflicts.push(format!(
                "{label} doesn't support the common loader(s): declares {}",
                loaders.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if !loader_sets.is_empty() && common_loaders.is_empty() {
        conflicts.push("No single loader supports all selected mods.".into());
    }
    for (label, gvs) in &version_sets {
        if !common_gv.is_empty() && gvs.is_disjoint(&common_gv.iter().cloned().collect()) {
            conflicts.push(format!(
                "{label} shares no MC version with the rest of the selection."
            ));
        }
    }
    if !version_sets.is_empty() && common_gv.is_empty() {
        conflicts.push("No single MC version satisfies all selected mods.".into());
    }

    // Suggested loader: most-voted among common loaders (or overall).
    let suggested_loader = common_loaders
        .iter()
        .max_by_key(|l| loader_votes.get(*l).copied().unwrap_or(0))
        .cloned()
        .or_else(|| {
            loader_votes
                .iter()
                .max_by_key(|(_, n)| *n)
                .map(|(l, _)| l.clone())
        });
    let suggested_mc_version = common_gv.first().cloned();

    CompatReport {
        suggested_mc_version,
        suggested_loader,
        common_game_versions: common_gv,
        common_loaders,
        conflicts,
        missing_deps: vec![],
    }
}

fn version_key(v: &str) -> (u64, u64, u64) {
    let mut it = v.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
