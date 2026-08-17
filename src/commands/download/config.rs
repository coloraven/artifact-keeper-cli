//! `download.config` — declarative multi-ecosystem ferry download plan.

use std::path::{Path, PathBuf};

use miette::Result;
use serde::Deserialize;

use super::Ecosystem;
use crate::error::AkError;

pub const DEFAULT_CONFIG_NAME: &str = "download.config";

/// Root of `download.config` (TOML).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadConfigFile {
    /// Directory for job outputs when `output` is relative (default: config file dir)
    pub output_dir: Option<PathBuf>,

    /// Shared work directory (kept after success). If unset, each job uses a temp dir.
    pub work_dir: Option<PathBuf>,

    /// SQLite download cache path (relative to config file dir if not absolute)
    pub cache_db: Option<PathBuf>,

    /// Force re-download ignoring the SQLite cache
    #[serde(default)]
    pub force: bool,

    /// Max concurrent toolchain workers (default: CPU count, max 16)
    pub jobs: Option<usize>,

    /// Server catalog JSONL from `ak download catalog` (skip packages already on server)
    pub catalog: Option<PathBuf>,

    /// One or more download jobs (preferred for multi-language ferry packs).
    #[serde(default)]
    pub downloads: Vec<DownloadJob>,

    // --- single-job shorthand (when `downloads` is empty) ---
    pub ecosystem: Option<String>,
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
    #[serde(default)]
    pub all_versions: bool,
    #[serde(default)]
    pub targets: Vec<String>,

    /// npm Node versions to target (e.g. `18`, `20.11.0`)
    #[serde(default)]
    pub nodes: Vec<String>,

    /// Go: `GOPROXY` (e.g. `https://goproxy.cn,direct`). npm: ignored.
    pub proxy: Option<String>,

    /// Go: `GOSUMDB` (e.g. `sum.golang.google.cn` or `off`).
    pub sumdb: Option<String>,

    /// Registry / index URL: npm, pypi (`--index-url`), or cargo sparse index.
    /// go: ignored (use `proxy` / `sumdb`).
    pub registry: Option<String>,

    /// Forward-compat bag for ecosystem-specific knobs (also allowed per-job).
    #[serde(default)]
    pub options: toml::Table,
}

/// A single ecosystem download job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadJob {
    /// `npm`, `go`, `pypi`, `cargo`
    pub ecosystem: String,

    /// Module list / lockfile / manifest path (relative to the config file directory)
    pub input: PathBuf,

    /// Output zip (relative to `output_dir` or the config directory)
    pub output: Option<PathBuf>,

    #[serde(default)]
    pub all_versions: bool,

    /// npm: OS-CPU targets. pypi: pip `--platform` tags (e.g. `manylinux2014_x86_64`).
    #[serde(default)]
    pub targets: Vec<String>,

    /// npm: Node versions. pypi: Python versions for cross-platform download (e.g. `3.11`, `3.12`).
    #[serde(default)]
    pub nodes: Vec<String>,

    /// Go module proxy (`GOPROXY`), e.g. `https://goproxy.cn,direct`
    pub proxy: Option<String>,

    /// Go checksum DB (`GOSUMDB`), e.g. `sum.golang.google.cn`
    pub sumdb: Option<String>,

    /// npm registry / pypi index-url / cargo sparse registry URL
    pub registry: Option<String>,

    /// Reserved per-ecosystem knobs
    #[serde(default)]
    pub options: toml::Table,
}

#[derive(Debug, Clone)]
pub struct ResolvedJob {
    pub ecosystem: Ecosystem,
    pub input: PathBuf,
    pub output: PathBuf,
    pub all_versions: bool,
    pub targets: Vec<String>,
    pub nodes: Vec<String>,
    pub proxy: Option<String>,
    pub sumdb: Option<String>,
    pub registry: Option<String>,
    pub options: toml::Table,
}

/// Upstream mirrors / registries for one download job.
#[derive(Debug, Clone, Default)]
pub struct UpstreamConfig {
    /// Go `GOPROXY`
    pub proxy: Option<String>,
    /// Go `GOSUMDB`
    pub sumdb: Option<String>,
    /// npm / pypi / cargo registry or index URL
    pub registry: Option<String>,
}

impl UpstreamConfig {
    pub fn from_job(job: &ResolvedJob) -> Self {
        Self {
            proxy: nonempty(job.proxy.as_deref()),
            sumdb: nonempty(job.sumdb.as_deref()),
            registry: nonempty(job.registry.as_deref()),
        }
    }

    /// Config → env → China-friendly default.
    pub fn goproxy(&self) -> String {
        self.proxy
            .clone()
            .or_else(|| std::env::var("GOPROXY").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "https://goproxy.cn,direct".into())
    }

    pub fn gosumdb(&self) -> String {
        self.sumdb
            .clone()
            .or_else(|| std::env::var("GOSUMDB").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "sum.golang.google.cn".into())
    }

    pub fn npm_registry(&self) -> String {
        self.registry
            .clone()
            .or_else(|| {
                std::env::var("npm_config_registry")
                    .ok()
                    .or_else(|| std::env::var("NPM_CONFIG_REGISTRY").ok())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "https://registry.npmmirror.com".into())
    }

    /// pip `--index-url` (config → `PIP_INDEX_URL` → Tuna).
    pub fn pypi_index(&self) -> String {
        self.registry
            .clone()
            .or_else(|| std::env::var("PIP_INDEX_URL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "https://pypi.tuna.tsinghua.edu.cn/simple".into())
    }

    /// Cargo sparse registry URL (with or without `sparse+` prefix).
    pub fn cargo_sparse_registry(&self) -> String {
        let raw = self
            .registry
            .clone()
            .or_else(|| std::env::var("CARGO_REGISTRIES_CRATES_IO_INDEX").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/".into()
            });
        if raw.starts_with("sparse+") || raw.starts_with("https://") || raw.starts_with("http://")
        {
            if raw.starts_with("sparse+") {
                raw
            } else {
                format!("sparse+{raw}")
            }
        } else {
            format!("sparse+{raw}")
        }
    }

    /// HTTP base for direct `.crate` downloads (all-versions). Config `registry` host
    /// is not used; prefer Tuna static mirror, else crates.io.
    pub fn cargo_crate_dl_base(&self) -> String {
        std::env::var("CARGO_CRATE_DL_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                "https://mirrors.tuna.tsinghua.edu.cn/crates.io/crates".into()
            })
    }
}

fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
}

impl DownloadConfigFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            AkError::ConfigError(format!("Cannot read {}: {e}", path.display()))
        })?;
        toml::from_str(&text).map_err(|e| {
            AkError::ConfigError(format!("Invalid {}: {e}", path.display())).into()
        })
    }

    /// Expand shorthand / `[[downloads]]` into concrete jobs with absolute-ish paths.
    pub fn resolve_jobs(&self, config_path: &Path) -> Result<Vec<ResolvedJob>> {
        let base = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let output_dir = self
            .output_dir
            .as_ref()
            .map(|p| resolve_against(base, p))
            .unwrap_or_else(|| base.to_path_buf());

        let raw_jobs: Vec<DownloadJob> = if !self.downloads.is_empty() {
            self.downloads.clone()
        } else if let (Some(eco), Some(input)) = (&self.ecosystem, &self.input) {
            vec![DownloadJob {
                ecosystem: eco.clone(),
                input: input.clone(),
                output: self.output.clone(),
                all_versions: self.all_versions,
                targets: self.targets.clone(),
                nodes: self.nodes.clone(),
                proxy: self.proxy.clone(),
                sumdb: self.sumdb.clone(),
                registry: self.registry.clone(),
                options: self.options.clone(),
            }]
        } else {
            return Err(AkError::ConfigError(format!(
                "{}: provide [[downloads]] jobs, or top-level ecosystem + input",
                config_path.display()
            ))
            .into());
        };

        let mut out = Vec::with_capacity(raw_jobs.len());
        for (i, job) in raw_jobs.into_iter().enumerate() {
            let ecosystem = parse_ecosystem(&job.ecosystem)?;
            let input = resolve_against(base, &job.input);
            let output = match &job.output {
                Some(p) => resolve_against(&output_dir, p),
                None => output_dir.join(format!(
                    "ak-ferry-{}-{}.zip",
                    job.ecosystem.to_ascii_lowercase(),
                    i + 1
                )),
            };
            out.push(ResolvedJob {
                ecosystem,
                input,
                output,
                all_versions: job.all_versions,
                targets: job.targets,
                nodes: job.nodes,
                proxy: job.proxy,
                sumdb: job.sumdb,
                registry: job.registry,
                options: job.options,
            });
        }
        Ok(out)
    }
}

fn resolve_against(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

pub fn parse_ecosystem(s: &str) -> Result<Ecosystem> {
    match s.trim().to_ascii_lowercase().as_str() {
        "go" => Ok(Ecosystem::Go),
        "npm" => Ok(Ecosystem::Npm),
        "pypi" | "pip" | "python" => Ok(Ecosystem::Pypi),
        "cargo" | "crates" | "rust" => Ok(Ecosystem::Cargo),
        other => Err(AkError::ConfigError(format!(
            "Unsupported ecosystem '{other}' in download.config \
             (supported: go, npm, pypi, cargo)"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multi_job_config() {
        let text = r#"
work_dir = "./work"
output_dir = "./out"

[[downloads]]
ecosystem = "npm"
input = "package.json"
output = "ferry-npm.zip"
targets = ["linux-x64", "darwin-arm64"]
nodes = ["18", "20"]
registry = "https://registry.npmmirror.com"

[[downloads]]
ecosystem = "go"
input = "go.mod"
all_versions = true
proxy = "https://goproxy.cn,direct"
sumdb = "sum.golang.google.cn"

[[downloads]]
ecosystem = "pypi"
input = "requirements.txt"
registry = "https://pypi.tuna.tsinghua.edu.cn/simple"

[[downloads]]
ecosystem = "cargo"
input = "Cargo.toml"
registry = "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/"
"#;
        let cfg: DownloadConfigFile = toml::from_str(text).unwrap();
        assert_eq!(cfg.downloads.len(), 4);
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("download.config");
        std::fs::write(&cfg_path, text).unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        std::fs::write(tmp.path().join("requirements.txt"), "requests==2.31.0\n").unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();
        let jobs = cfg.resolve_jobs(&cfg_path).unwrap();
        assert_eq!(jobs.len(), 4);
        assert_eq!(jobs[0].ecosystem, Ecosystem::Npm);
        assert_eq!(jobs[2].ecosystem, Ecosystem::Pypi);
        assert_eq!(jobs[3].ecosystem, Ecosystem::Cargo);
        assert_eq!(
            UpstreamConfig::from_job(&jobs[2]).pypi_index(),
            "https://pypi.tuna.tsinghua.edu.cn/simple"
        );
    }

    #[test]
    fn parse_shorthand_single_job() {
        let text = r#"
ecosystem = "npm"
input = "pkgs.txt"
output = "all.zip"
all_versions = true
"#;
        let cfg: DownloadConfigFile = toml::from_str(text).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("download.config");
        std::fs::write(&cfg_path, text).unwrap();
        let jobs = cfg.resolve_jobs(&cfg_path).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].all_versions);
        assert!(jobs[0].output.ends_with("all.zip"));
    }
}
