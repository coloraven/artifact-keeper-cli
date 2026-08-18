//! Recognize an `ak download --no-archive` tree and map package files to the
//! same artifact paths server `ferry_ingest_service` uses.

use std::path::{Path, PathBuf};

use miette::Result;

use super::download::manifest::{FerryManifest, ModuleEntry, MANIFEST_JSON, MANIFEST_JSONL};
use super::download::parse_ecosystem;
use super::download::Ecosystem;
use crate::error::AkError;

#[derive(Debug, Clone)]
pub struct FerryUploadItem {
    pub local_path: PathBuf,
    pub artifact_path: String,
}

#[derive(Debug, Clone)]
pub struct FerryPushPlan {
    pub ecosystem: String,
    /// When set, upload via the Go module-proxy protocol from this cache tree.
    pub go_download_root: Option<PathBuf>,
    pub items: Vec<FerryUploadItem>,
}

/// True when `dir` contains `ak-ferry.json` or `ak-ferry.jsonl`.
pub fn detect_ferry_root(dir: &Path) -> bool {
    dir.join(MANIFEST_JSON).is_file() || dir.join(MANIFEST_JSONL).is_file()
}

/// Parse a ferry directory into protocol-correct upload items.
/// Returns `Ok(None)` when `dir` is not a ferry root.
pub fn plan_ferry_push(dir: &Path) -> Result<Option<FerryPushPlan>> {
    if !detect_ferry_root(dir) {
        return Ok(None);
    }
    let manifest = load_ferry_manifest(dir)?;
    let eco = parse_ecosystem(&manifest.ecosystem)?;
    match eco {
        Ecosystem::Go => {
            let download = dir.join("download");
            let go_root = if download.is_dir() {
                download
            } else {
                dir.to_path_buf()
            };
            Ok(Some(FerryPushPlan {
                ecosystem: "go".into(),
                go_download_root: Some(go_root),
                items: Vec::new(),
            }))
        }
        Ecosystem::Npm | Ecosystem::Pypi | Ecosystem::Cargo => {
            let items = collect_items(dir, &manifest)?;
            if items.is_empty() {
                return Err(AkError::ConfigError(format!(
                    "Ferry dir {} has no uploadable package files in the manifest",
                    dir.display()
                ))
                .into());
            }
            Ok(Some(FerryPushPlan {
                ecosystem: manifest.ecosystem,
                go_download_root: None,
                items,
            }))
        }
    }
}

fn load_ferry_manifest(dir: &Path) -> Result<FerryManifest> {
    let json = dir.join(MANIFEST_JSON);
    if json.is_file() {
        let text = std::fs::read_to_string(&json).map_err(|e| {
            AkError::ConfigError(format!("Cannot read {}: {e}", json.display()))
        })?;
        return serde_json::from_str(&text).map_err(|e| {
            AkError::ConfigError(format!("Invalid {}: {e}", json.display())).into()
        });
    }
    let jsonl = dir.join(MANIFEST_JSONL);
    let modules = load_jsonl_modules(&jsonl)?;
    let eco = infer_ecosystem(dir, &modules).ok_or_else(|| {
        AkError::ConfigError(format!(
            "Cannot infer ecosystem from {} (add ak-ferry.json)",
            jsonl.display()
        ))
    })?;
    let mut manifest = FerryManifest::new(&eco, Vec::new());
    manifest.modules = modules;
    Ok(manifest)
}

fn load_jsonl_modules(path: &Path) -> Result<Vec<ModuleEntry>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        AkError::ConfigError(format!("Cannot read {}: {e}", path.display()))
    })?;
    let mut modules = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: ModuleEntry = serde_json::from_str(line).map_err(|e| {
            AkError::ConfigError(format!(
                "Invalid {} line {}: {e}",
                path.display(),
                i + 1
            ))
        })?;
        modules.push(entry);
    }
    Ok(modules)
}

fn infer_ecosystem(dir: &Path, modules: &[ModuleEntry]) -> Option<String> {
    if dir.join("download").is_dir() {
        return Some("go".into());
    }
    if dir.join("npm").is_dir() {
        return Some("npm".into());
    }
    if dir.join("pypi").is_dir() {
        return Some("pypi".into());
    }
    if dir.join("cargo").is_dir() {
        return Some("cargo".into());
    }
    for m in modules {
        for f in &m.files {
            let p = f.relpath.replace('\\', "/");
            if p.ends_with(".tgz") {
                return Some("npm".into());
            }
            if p.ends_with(".crate") {
                return Some("cargo".into());
            }
            if p.ends_with(".whl") || p.ends_with(".tar.gz") {
                return Some("pypi".into());
            }
            if p.contains("/@v/") {
                return Some("go".into());
            }
        }
    }
    None
}

fn collect_items(dir: &Path, manifest: &FerryManifest) -> Result<Vec<FerryUploadItem>> {
    let eco = parse_ecosystem(&manifest.ecosystem)?;
    let mut items = Vec::new();
    for module in &manifest.modules {
        for file in &module.files {
            let rel = file.relpath.replace('\\', "/");
            if rel == MANIFEST_JSON || rel == MANIFEST_JSONL || rel.ends_with("/ak-ferry.json")
            {
                continue;
            }
            let local = join_rel(dir, &rel);
            if !local.is_file() {
                continue;
            }
            let filename = local
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("package")
                .to_string();
            let artifact_path = match eco {
                Ecosystem::Npm => npm_artifact_path(&module.name, &module.version, &filename),
                Ecosystem::Pypi => pypi_artifact_path(&module.name, &module.version, &filename),
                Ecosystem::Cargo => cargo_artifact_path(&module.name, &module.version, &filename),
                Ecosystem::Go => continue,
            };
            items.push(FerryUploadItem {
                local_path: local,
                artifact_path,
            });
        }
    }
    Ok(items)
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        p.push(part);
    }
    p
}

fn ensure_suffix(name: &str, suffix: &str) -> String {
    if name.ends_with(suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

/// `{name}/{version}/{filename}.tgz` — matches `store_npm_module`.
pub fn npm_artifact_path(name: &str, version: &str, filename: &str) -> String {
    let filename = ensure_suffix(filename, ".tgz");
    format!("{name}/{version}/{filename}")
}

/// PEP 503 normalize, then `{normalized}/{version}/{filename}`.
pub fn normalize_pypi_name(name: &str) -> String {
    let mut result = String::new();
    let mut last_was_separator = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            result.push('-');
            last_was_separator = true;
        }
    }
    if result.ends_with('-') {
        result.pop();
    }
    result
}

pub fn pypi_artifact_path(name: &str, version: &str, filename: &str) -> String {
    format!("{}/{version}/{filename}", normalize_pypi_name(name))
}

/// `{name_lower}/{version}/{filename}.crate` — matches `store_cargo_module`.
pub fn cargo_artifact_path(name: &str, version: &str, filename: &str) -> String {
    let name_lower = name.to_ascii_lowercase();
    let filename = if filename.ends_with(".crate") {
        filename.to_string()
    } else {
        ensure_suffix(&format!("{name_lower}-{version}"), ".crate")
    };
    format!("{name_lower}/{version}/{filename}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::download::manifest::{FileEntry, RootSpec};

    #[test]
    fn detect_ferry_root_json_and_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!detect_ferry_root(tmp.path()));
        std::fs::write(tmp.path().join("ak-ferry.jsonl"), "{}\n").unwrap();
        assert!(detect_ferry_root(tmp.path()));
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp2.path().join("ak-ferry.json"), "{}").unwrap();
        assert!(detect_ferry_root(tmp2.path()));
    }

    #[test]
    fn detect_skips_plain_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.tgz"), b"x").unwrap();
        assert!(!detect_ferry_root(tmp.path()));
    }

    #[test]
    fn npm_path_formula() {
        assert_eq!(
            npm_artifact_path("lodash", "4.17.21", "lodash-4.17.21.tgz"),
            "lodash/4.17.21/lodash-4.17.21.tgz"
        );
        assert_eq!(
            npm_artifact_path("@scope/pkg", "1.0.0", "pkg-1.0.0"),
            "@scope/pkg/1.0.0/pkg-1.0.0.tgz"
        );
    }

    #[test]
    fn pypi_path_formula() {
        assert_eq!(
            pypi_artifact_path("Jinja2", "3.1.4", "Jinja2-3.1.4-py3-none-any.whl"),
            "jinja2/3.1.4/Jinja2-3.1.4-py3-none-any.whl"
        );
        assert_eq!(normalize_pypi_name("friendly_id"), "friendly-id");
    }

    #[test]
    fn cargo_path_formula() {
        assert_eq!(
            cargo_artifact_path("Serde", "1.0.210", "serde-1.0.210.crate"),
            "serde/1.0.210/serde-1.0.210.crate"
        );
        assert_eq!(
            cargo_artifact_path("serde", "1.0.0", "blob"),
            "serde/1.0.0/serde-1.0.0.crate"
        );
    }

    #[test]
    fn plan_npm_ferry_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tgz = tmp.path().join("npm").join("lodash-4.17.21.tgz");
        std::fs::create_dir_all(tgz.parent().unwrap()).unwrap();
        std::fs::write(&tgz, b"tgz").unwrap();
        let mut manifest = FerryManifest::new(
            "npm",
            vec![RootSpec {
                name: "lodash".into(),
                version: Some("4.17.21".into()),
            }],
        );
        manifest.modules.push(ModuleEntry {
            name: "lodash".into(),
            name_encoded: None,
            version: "4.17.21".into(),
            files: vec![FileEntry {
                relpath: "npm/lodash-4.17.21.tgz".into(),
                sha256: "ab".into(),
                size: 3,
            }],
            via: "lodash".into(),
        });
        manifest.save(tmp.path()).unwrap();

        let plan = plan_ferry_push(tmp.path()).unwrap().unwrap();
        assert_eq!(plan.ecosystem, "npm");
        assert!(plan.go_download_root.is_none());
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].artifact_path, "lodash/4.17.21/lodash-4.17.21.tgz");
    }

    #[test]
    fn plan_go_ferry_wraps_download() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = FerryManifest::new("go", vec![]);
        manifest.save(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join("download")).unwrap();
        let plan = plan_ferry_push(tmp.path()).unwrap().unwrap();
        assert_eq!(plan.ecosystem, "go");
        assert_eq!(
            plan.go_download_root.as_deref(),
            Some(tmp.path().join("download").as_path())
        );
    }
}
