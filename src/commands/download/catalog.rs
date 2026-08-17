//! Server repository catalog: export from AK API + load for ferry skip.
//!
//! CLI export entry point: `ak repo catalog <repo>`.
//! Ferry download consumes the JSONL via `ak download --catalog <file>`.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use miette::Result;
use serde::{Deserialize, Serialize};

use super::cargo;
use super::pypi;
use crate::cli::GlobalArgs;
use crate::commands::client::client_for_optional_auth;
use crate::commands::helpers::sdk_err;
use crate::commands::go_proxy;
use crate::error::AkError;
use crate::output::OutputFormat;
use artifact_keeper_sdk::{ClientPackagesExt, ClientRepositoriesExt};

/// One skippable package/module already present on the (intranet) server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// `packages` or `artifacts`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// In-memory index used by `ak download --catalog` to skip modules.
#[derive(Debug, Default, Clone)]
pub struct ServerCatalog {
    /// (ecosystem, normalized_name, version)
    keys: HashSet<(String, String, String)>,
}

impl ServerCatalog {
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| {
            AkError::ConfigError(format!("Cannot read catalog {}: {e}", path.display()))
        })?;
        let reader = BufReader::new(file);
        let mut keys = HashSet::new();
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                AkError::ConfigError(format!("Read {}:{}: {e}", path.display(), lineno + 1))
            })?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let entry: CatalogEntry = serde_json::from_str(line).map_err(|e| {
                AkError::ConfigError(format!(
                    "Invalid catalog JSON at {}:{}: {e}",
                    path.display(),
                    lineno + 1
                ))
            })?;
            let eco = normalize_ecosystem(&entry.ecosystem)?;
            let name = normalize_name(&eco, &entry.name);
            let ver = entry.version.trim().to_string();
            if ver.is_empty() {
                continue;
            }
            keys.insert((eco, name, ver));
        }
        Ok(Self { keys })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// True when this package/module version is listed as already on the server.
    pub fn contains(&self, ecosystem: &str, name: &str, version: &str) -> bool {
        let Ok(eco) = normalize_ecosystem(ecosystem) else {
            return false;
        };
        let name = normalize_name(&eco, name);
        let ver = version.trim();
        if ver.is_empty() {
            return false;
        }
        self.keys.contains(&(eco, name, ver.to_string()))
    }

    /// Like [`contains`], but never skips when `force` is set.
    pub fn should_skip(&self, force: bool, ecosystem: &str, name: &str, version: &str) -> bool {
        !force && self.contains(ecosystem, name, version)
    }
}

pub fn normalize_ecosystem(s: &str) -> Result<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "npm" => Ok("npm".into()),
        "go" | "golang" => Ok("go".into()),
        "pypi" | "pip" | "python" => Ok("pypi".into()),
        "cargo" | "crates" | "crate" | "rust" => Ok("cargo".into()),
        other => Err(AkError::ConfigError(format!(
            "Unsupported catalog ecosystem '{other}' (use npm, go, pypi, cargo)"
        ))
        .into()),
    }
}

fn normalize_name(ecosystem: &str, name: &str) -> String {
    match ecosystem {
        "npm" | "pypi" => name
            .chars()
            .map(|c| {
                if c == '_' || c == '.' {
                    '-'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
        _ => name.to_string(),
    }
}

fn format_to_ecosystem(fmt: &str) -> Option<&'static str> {
    match fmt.trim().to_ascii_lowercase().as_str() {
        "npm" => Some("npm"),
        "go" | "golang" => Some("go"),
        "pypi" | "python" => Some("pypi"),
        "cargo" | "crates" | "crate" => Some("cargo"),
        _ => None,
    }
}

/// Export server package inventory to JSONL for offline ferry skip.
pub async fn export_catalog(
    repo: &str,
    output: &Path,
    formats: &[String],
    include_artifacts: bool,
    global: &GlobalArgs,
) -> Result<()> {
    let allow: Option<HashSet<String>> = if formats.is_empty() {
        None
    } else {
        let mut set = HashSet::new();
        for f in formats {
            set.insert(normalize_ecosystem(f)?);
        }
        Some(set)
    };

    let client = client_for_optional_auth(global)?;
    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut seen = HashSet::<(String, String, String)>::new();

    // --- packages API (name + version + format) ---
    let mut page = 1_i32;
    let per_page = 100_i32;
    loop {
        let mut req = client
            .list_packages()
            .repository_key(repo)
            .page(page)
            .per_page(per_page);
        // If exactly one format filter, push it server-side.
        if let Some(ref allow) = allow {
            if allow.len() == 1 {
                let fmt = allow.iter().next().unwrap().clone();
                req = req.format(fmt);
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| sdk_err("list packages for catalog", e))?;

        for p in &resp.items {
            let Some(eco) = format_to_ecosystem(&p.format) else {
                continue;
            };
            if let Some(ref allow) = allow {
                if !allow.contains(eco) {
                    continue;
                }
            }
            let name = normalize_name(eco, &p.name);
            let ver = p.version.trim().to_string();
            if ver.is_empty() {
                continue;
            }
            let key = (eco.to_string(), name.clone(), ver.clone());
            if !seen.insert(key) {
                continue;
            }
            entries.push(CatalogEntry {
                ecosystem: eco.into(),
                name,
                version: ver,
                repository_key: Some(p.repository_key.clone()),
                path: None,
                sha256: None,
                source: Some("packages".into()),
            });
        }

        if resp.pagination.total_pages == 0 || page >= resp.pagination.total_pages {
            break;
        }
        page += 1;
    }

    // --- optional artifacts scrape (Go proxy paths, etc.) ---
    if include_artifacts {
        let mut page = 1_i32;
        loop {
            let resp = client
                .list_artifacts()
                .key(repo)
                .page(page)
                .per_page(per_page)
                .send()
                .await
                .map_err(|e| sdk_err("list artifacts for catalog", e))?;

            for a in &resp.items {
                if let Some(entry) = entry_from_artifact(a, allow.as_ref()) {
                    let key = (
                        entry.ecosystem.clone(),
                        entry.name.clone(),
                        entry.version.clone(),
                    );
                    if seen.insert(key) {
                        entries.push(entry);
                    }
                }
            }

            if resp.pagination.total_pages == 0 || page >= resp.pagination.total_pages {
                break;
            }
            page += 1;
        }
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AkError::ConfigError(format!("Cannot create {}: {e}", parent.display()))
            })?;
        }
    }

    let mut file = std::fs::File::create(output).map_err(|e| {
        AkError::ConfigError(format!("Cannot write catalog {}: {e}", output.display()))
    })?;
    writeln!(
        file,
        "# ak-catalog v1 repo={repo} entries={} exported_at={}",
        entries.len(),
        chrono::Utc::now().to_rfc3339()
    )
    .map_err(|e| AkError::ConfigError(format!("Write catalog: {e}")))?;

    for entry in &entries {
        let line = serde_json::to_string(entry)
            .map_err(|e| AkError::ConfigError(format!("Serialize catalog entry: {e}")))?;
        writeln!(file, "{line}")
            .map_err(|e| AkError::ConfigError(format!("Write catalog: {e}")))?;
    }

    if matches!(global.format, OutputFormat::Quiet) {
        println!("{}", output.display());
    } else {
        eprintln!(
            "Wrote {} catalog entries -> {} (take this file to the internet host and pass --catalog)",
            entries.len(),
            output.display()
        );
    }
    Ok(())
}

fn entry_from_artifact(
    a: &artifact_keeper_sdk::types::ArtifactResponse,
    allow: Option<&HashSet<String>>,
) -> Option<CatalogEntry> {
    // Prefer explicit version + name when the API filled them and format is known via path.
    if let Some((eco, name, version)) = parse_artifact_identity(a) {
        if let Some(allow) = allow {
            if !allow.contains(eco) {
                return None;
            }
        }
        return Some(CatalogEntry {
            ecosystem: eco.into(),
            name: normalize_name(eco, &name),
            version,
            repository_key: Some(a.repository_key.clone()),
            path: Some(a.path.clone()),
            sha256: Some(a.checksum_sha256.clone()),
            source: Some("artifacts".into()),
        });
    }
    None
}

/// Best-effort map of artifact path → (ecosystem, name, version).
fn parse_artifact_identity(
    a: &artifact_keeper_sdk::types::ArtifactResponse,
) -> Option<(&'static str, String, String)> {
    let path = a.path.replace('\\', "/");

    // Go module proxy: …/<module>/@v/<version>.zip|.mod|.info
    if let Some((module_part, file)) = path.rsplit_once("/@v/") {
        if let Some(version) = file
            .strip_suffix(".zip")
            .or_else(|| file.strip_suffix(".mod"))
            .or_else(|| file.strip_suffix(".info"))
        {
            let encoded = module_part.rsplit('/').collect::<Vec<_>>();
            // module path may include nested dirs; take everything before /@v/
            let module_encoded = if let Some(idx) = path.find("/@v/") {
                // strip leading ferry prefixes like "download/"
                let raw = &path[..idx];
                raw.strip_prefix("download/")
                    .unwrap_or(raw)
                    .trim_start_matches('/')
            } else {
                encoded.last().copied().unwrap_or("")
            };
            if !module_encoded.is_empty() && !version.is_empty() {
                let name = go_proxy::decode_go_path(module_encoded);
                return Some(("go", name, version.to_string()));
            }
        }
    }

    // npm tarball: …/name/-/name-version.tgz or @scope/name/-/name-version.tgz
    if path.ends_with(".tgz") {
        if let Some((name, ver)) = parse_npm_tarball_path(&path) {
            return Some(("npm", name, ver));
        }
        if let (Some(name), Some(ver)) = (nonzero(&a.name), a.version.as_deref()) {
            return Some(("npm", name, ver.to_string()));
        }
    }

    // PyPI wheel / sdist
    if path.ends_with(".whl") || path.ends_with(".tar.gz") || path.ends_with(".zip") {
        if let Some(fname) = path.rsplit('/').next() {
            if let Some((name, ver)) = pypi::parse_dist_filename(fname) {
                return Some(("pypi", name, ver));
            }
        }
    }

    // Cargo .crate
    if path.ends_with(".crate") {
        if let Some(fname) = path.rsplit('/').next() {
            if let Some((name, ver)) = cargo::parse_crate_filename(fname) {
                return Some(("cargo", name, ver));
            }
        }
    }

    // Generic: trust API name+version when present
    if let (Some(name), Some(ver)) = (nonzero(&a.name), a.version.as_deref()) {
        // Unknown ecosystem — skip rather than guess wrong.
        let _ = (name, ver);
    }
    None
}

fn nonzero(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn parse_npm_tarball_path(path: &str) -> Option<(String, String)> {
    // …/@scope/pkg/-/pkg-1.2.3.tgz or …/pkg/-/pkg-1.2.3.tgz
    let (name_part, file_part) = path.split_once("/-/")?;
    let file = file_part.strip_suffix(".tgz")?;
    let pkg_name = if name_part.contains("/@") || name_part.contains('@') {
        // find @scope/name
        if let Some(idx) = name_part.rfind("/@") {
            name_part[idx + 1..].to_string()
        } else if let Some(idx) = name_part.find('@') {
            name_part[idx..].to_string()
        } else {
            name_part.rsplit('/').next()?.to_string()
        }
    } else {
        name_part.rsplit('/').next()?.to_string()
    };
    let unscoped = pkg_name.rsplit('/').next()?;
    let version = file.strip_prefix(&format!("{unscoped}-"))?;
    if version.is_empty() {
        return None;
    }
    Some((pkg_name, version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ecosystems() {
        assert_eq!(normalize_ecosystem("NPM").unwrap(), "npm");
        assert_eq!(normalize_ecosystem("golang").unwrap(), "go");
        assert_eq!(normalize_ecosystem("pip").unwrap(), "pypi");
        assert_eq!(normalize_ecosystem("crates").unwrap(), "cargo");
    }

    #[test]
    fn catalog_roundtrip_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.jsonl");
        std::fs::write(
            &path,
            r#"# comment
{"ecosystem":"npm","name":"Lodash","version":"4.17.21"}
{"ecosystem":"go","name":"github.com/foo/bar","version":"v1.2.3"}
"#,
        )
        .unwrap();
        let cat = ServerCatalog::load(&path).unwrap();
        assert_eq!(cat.len(), 2);
        assert!(cat.contains("npm", "lodash", "4.17.21"));
        assert!(cat.contains("go", "github.com/foo/bar", "v1.2.3"));
        assert!(!cat.contains("npm", "lodash", "1.0.0"));
    }

    #[test]
    fn parse_npm_paths() {
        let (n, v) = parse_npm_tarball_path("npm/lodash/-/lodash-4.17.21.tgz").unwrap();
        assert_eq!(n, "lodash");
        assert_eq!(v, "4.17.21");
        let (n, v) = parse_npm_tarball_path("@scope/pkg/-/pkg-1.0.0.tgz").unwrap();
        assert_eq!(n, "@scope/pkg");
        assert_eq!(v, "1.0.0");
    }
}
