//! Ferry pack manifest (`ak-ferry.json` + append-only `ak-ferry.jsonl`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use miette::Result;
use serde::{Deserialize, Serialize};

use crate::error::AkError;

pub const MANIFEST_JSON: &str = "ak-ferry.json";
pub const MANIFEST_JSONL: &str = "ak-ferry.jsonl";
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FerryManifest {
    pub version: u32,
    pub kind: String,
    pub ecosystem: String,
    pub created_at: String,
    pub updated_at: String,
    pub roots: Vec<RootSpec>,
    pub modules: Vec<ModuleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleEntry {
    /// Display / decoded module or package name
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_encoded: Option<String>,
    pub version: String,
    /// Paths relative to the ferry payload root (inside the zip)
    pub files: Vec<FileEntry>,
    /// Which user root pulled this in (best-effort)
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub relpath: String,
    pub sha256: String,
    pub size: u64,
}

impl FerryManifest {
    pub fn new(ecosystem: &str, roots: Vec<RootSpec>) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            version: MANIFEST_VERSION,
            kind: "ak-ferry".into(),
            ecosystem: ecosystem.into(),
            created_at: now.clone(),
            updated_at: now,
            roots,
            modules: Vec::new(),
        }
    }

    pub fn load_or_create(dir: &Path, ecosystem: &str, roots: Vec<RootSpec>) -> Result<Self> {
        let path = dir.join(MANIFEST_JSON);
        if path.is_file() {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                AkError::ConfigError(format!("Cannot read {}: {e}", path.display()))
            })?;
            serde_json::from_str(&text).map_err(|e| {
                AkError::ConfigError(format!("Invalid {}: {e}", path.display())).into()
            })
        } else {
            let m = Self::new(ecosystem, roots);
            m.save(dir)?;
            Ok(m)
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_JSON);
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| AkError::ConfigError(format!("Serialize manifest: {e}")))?;
        std::fs::write(&path, text)
            .map_err(|e| AkError::ConfigError(format!("Write {}: {e}", path.display())))?;
        Ok(())
    }

    /// Append one JSON line and refresh `ak-ferry.json` (crash-safe incremental index).
    pub fn record_module(&mut self, dir: &Path, entry: ModuleEntry) -> Result<()> {
        // Dedupe by name+version
        if self
            .modules
            .iter()
            .any(|m| m.name == entry.name && m.version == entry.version)
        {
            return Ok(());
        }

        let line = serde_json::to_string(&entry)
            .map_err(|e| AkError::ConfigError(format!("Serialize module entry: {e}")))?;
        let jsonl = dir.join(MANIFEST_JSONL);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl)
            .map_err(|e| AkError::ConfigError(format!("Open {}: {e}", jsonl.display())))?;
        writeln!(f, "{line}")
            .map_err(|e| AkError::ConfigError(format!("Write {}: {e}", jsonl.display())))?;

        self.modules.push(entry);
        self.updated_at = Utc::now().to_rfc3339();
        self.save(dir)
    }
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| AkError::ConfigError(format!("Open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher)
        .map_err(|e| AkError::ConfigError(format!("Hash {}: {e}", path.display())))?;
    Ok((hex::encode(hasher.finalize()), size))
}

pub fn file_entry(payload_root: &Path, abs: &Path) -> Result<FileEntry> {
    let rel = abs.strip_prefix(payload_root).map_err(|_| {
        AkError::ConfigError(format!(
            "{} is not under {}",
            abs.display(),
            payload_root.display()
        ))
    })?;
    let relpath = rel.to_string_lossy().replace('\\', "/");
    let (sha256, size) = sha256_file(abs)?;
    Ok(FileEntry {
        relpath,
        sha256,
        size,
    })
}

/// True when every listed file exists under `payload_root` with matching size and sha256.
/// On mismatch, deletes the bad file so the next fetch can rewrite it.
pub fn module_files_intact(payload_root: &Path, files: &[FileEntry]) -> bool {
    if files.is_empty() {
        return false;
    }
    for f in files {
        let path = join_rel(payload_root, &f.relpath);
        if !path.is_file() {
            return false;
        }
        match sha256_file(&path) {
            Ok((sha, size)) if sha == f.sha256 && size == f.size => {}
            _ => {
                let _ = std::fs::remove_file(&path);
                return false;
            }
        }
    }
    true
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.replace('\\', "/").split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        p.push(part);
    }
    p
}

/// Parse `name@version` / `name version` / bare name lines; `#` comments; blank skip.
/// Scoped npm packages: `@scope/name`, `@scope/name@version`, or `@scope/name 1.2.3`.
pub fn parse_module_list(text: &str) -> Vec<RootSpec> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // Whitespace form: "name version" / "@scope/name 1.2.3"
        let mut parts = line.split_whitespace();
        if let Some(first) = parts.next() {
            if let Some(ver) = parts.next() {
                out.push(RootSpec {
                    name: first.to_string(),
                    version: Some(ver.to_string()),
                });
                continue;
            }
            // Single token: name, name@version, or @scope/name@version
            if let Some(spec) = parse_at_spec(first) {
                out.push(spec);
            }
        }
    }
    out
}

fn parse_at_spec(token: &str) -> Option<RootSpec> {
    if token.starts_with('@') {
        // @scope/name or @scope/name@version (version after the second '@')
        let rest = &token[1..];
        if let Some((path, ver)) = rest.rsplit_once('@')
            && path.contains('/')
        {
            return Some(RootSpec {
                name: format!("@{path}"),
                version: if ver.is_empty() {
                    None
                } else {
                    Some(ver.to_string())
                },
            });
        }
        return Some(RootSpec {
            name: token.to_string(),
            version: None,
        });
    }
    if let Some((name, ver)) = token.split_once('@') {
        let name = name.trim();
        let ver = ver.trim();
        if name.is_empty() {
            return None;
        }
        return Some(RootSpec {
            name: name.to_string(),
            version: if ver.is_empty() {
                None
            } else {
                Some(ver.to_string())
            },
        });
    }
    Some(RootSpec {
        name: token.to_string(),
        version: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_variants() {
        let text = r#"
# comment
github.com/foo/bar v1.2.3
lodash@4.17.21
bare-module
@scope/pkg@1.2.3
@other/pkg 2.0.0
"#;
        let roots = parse_module_list(text);
        assert_eq!(roots.len(), 5);
        assert_eq!(roots[0].name, "github.com/foo/bar");
        assert_eq!(roots[0].version.as_deref(), Some("v1.2.3"));
        assert_eq!(roots[1].name, "lodash");
        assert_eq!(roots[1].version.as_deref(), Some("4.17.21"));
        assert_eq!(roots[2].name, "bare-module");
        assert!(roots[2].version.is_none());
        assert_eq!(roots[3].name, "@scope/pkg");
        assert_eq!(roots[3].version.as_deref(), Some("1.2.3"));
        assert_eq!(roots[4].name, "@other/pkg");
        assert_eq!(roots[4].version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn record_module_writes_json_and_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = FerryManifest::new(
            "go",
            vec![RootSpec {
                name: "github.com/foo/bar".into(),
                version: Some("v1.0.0".into()),
            }],
        );
        m.record_module(
            tmp.path(),
            ModuleEntry {
                name: "github.com/foo/bar".into(),
                name_encoded: Some("github.com/foo/bar".into()),
                version: "v1.0.0".into(),
                files: vec![],
                via: "github.com/foo/bar@v1.0.0".into(),
            },
        )
        .unwrap();
        assert!(tmp.path().join(MANIFEST_JSON).is_file());
        assert!(tmp.path().join(MANIFEST_JSONL).is_file());
        let reloaded = FerryManifest::load_or_create(tmp.path(), "go", vec![]).unwrap();
        assert_eq!(reloaded.modules.len(), 1);
    }

    #[test]
    fn module_files_intact_checks_sha_and_deletes_bad() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("pkg.tgz");
        std::fs::write(&f, b"hello").unwrap();
        let (sha, size) = sha256_file(&f).unwrap();
        let entry = FileEntry {
            relpath: "pkg.tgz".into(),
            sha256: sha.clone(),
            size,
        };
        assert!(module_files_intact(tmp.path(), &[entry.clone()]));
        std::fs::write(&f, b"nope").unwrap();
        assert!(!module_files_intact(
            tmp.path(),
            &[FileEntry {
                relpath: "pkg.tgz".into(),
                sha256: sha,
                size,
            }]
        ));
        assert!(!f.is_file());
    }
}
