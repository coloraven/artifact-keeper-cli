//! Offline ferry pack builder: isolate per root module, invoke the language
//! toolchain, stream a manifest, then zip for `ak artifact push --from-archive`.
//!
//! Parameters can come from CLI flags or a `download.config` TOML plan (preferred
//! as more ecosystems are added).

mod cache;
mod cargo;
mod catalog;
mod config;
mod engine;
mod errors;
mod go;
mod manifest;
mod naming;
mod npm;
mod pack;
mod parallel;
mod pypi;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use miette::Result;

use self::cache::{default_cache_db_path, DownloadCache};
use self::catalog::ServerCatalog;
use self::config::{DownloadConfigFile, ResolvedJob, UpstreamConfig, DEFAULT_CONFIG_NAME};
use crate::cli::GlobalArgs;
use crate::error::AkError;
use crate::output::OutputFormat;

// Re-export so new ecosystems (pypi/cargo/…) can implement the shared engine trait.
#[allow(unused_imports)]
pub use self::engine::{FerryOpts, LanguageToolchain, RootPass, WorkDirs};
pub use self::catalog::{default_catalog_output, export_catalog};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Go,
    Npm,
    Pypi,
    Cargo,
}

/// Mutually exclusive ecosystem flags (`--go` / `--npm` / `--pypi` / `--cargo`).
/// Not required when using `--config` / `download.config`.
#[derive(Args, Debug)]
#[group(required = false, multiple = false)]
pub struct EcosystemArgs {
    /// Download Go modules (`go get` / module cache), one root at a time
    #[arg(long = "go")]
    pub go: bool,

    /// Download npm packages (`npm install` / pack), one root at a time
    #[arg(long = "npm")]
    pub npm: bool,

    /// Download PyPI packages (`pip download`), one root at a time
    #[arg(long = "pypi")]
    pub pypi: bool,

    /// Download crates.io crates (`cargo fetch` / `.crate`), one root at a time
    #[arg(long = "cargo")]
    pub cargo: bool,
}

/// Build an air-gap ferry zip from a module list / lockfile using native toolchains.
#[derive(Args, Debug)]
pub struct DownloadArgs {
    #[command(flatten)]
    pub ecosystem: EcosystemArgs,

    /// Module list file, lockfile, or manifest (CLI mode; omit when using --config)
    ///
    /// List format (one per line): `name@version`, `name version`, or bare `name`.
    /// Lines starting with `#` are comments. For npm scoped packages use
    /// `@scope/name@1.2.3` or `@scope/name 1.2.3`.
    /// Also accepts: `go.mod`, `package.json`, `requirements.txt`, `Cargo.toml`.
    #[arg(required = false)]
    pub input: Option<PathBuf>,

    /// TOML download plan (multi-language). Default: `./download.config` when
    /// no ecosystem flag (`--go`/`--npm`/`--pypi`/`--cargo`) is set.
    #[arg(long = "config", value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Server catalog JSONL from `ak repo catalog` — skip packages already
    /// present on the intranet Artifact Keeper repo.
    #[arg(long = "catalog", value_name = "FILE")]
    pub catalog: Option<PathBuf>,

    /// Output zip path. When omitted, writes
    /// `ak-ferry-{ecosystem}-{timestamp}[-opts][-rN-mM].zip` in the current directory
    /// (or the config `output_dir`).
    #[arg(short, long, value_name = "OUTPUT")]
    pub output: Option<PathBuf>,

    /// Keep / reuse work directory during the run (default: temp).
    /// Downloaded modules under this dir are still deleted after the ferry zip is written.
    #[arg(long = "work-dir", value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Ignore pinned versions: fetch every published version of each root package/module
    ///
    /// npm: `npm view` + `npm pack` (root tarballs only).
    /// go: `go list -m -versions` + isolated `go get` (deps in shared GOMODCACHE).
    /// pypi: `pip index versions` + `pip download --no-deps` (root dists only).
    /// cargo: sparse index versions + direct `.crate` download (root crates only).
    #[arg(long = "all-versions")]
    pub all_versions: bool,

    /// Platform matrix (repeatable / comma-separated).
    ///
    /// npm: `OS-CPU` or `OS-CPU-LIBC` (e.g. `linux-x64`) → `npm install --os/--cpu`.
    /// pypi: pip `--platform` tag (e.g. `manylinux2014_x86_64`, `win_amd64`).
    /// go / cargo: ignored.
    #[arg(long = "target", value_name = "TARGET", action = clap::ArgAction::Append)]
    pub targets: Vec<String>,

    /// Runtime version matrix (repeatable / comma-separated).
    ///
    /// npm: Node versions (`18`, `20`) for ABI / prebuild.
    /// pypi: Python versions (`3.11`, `3.12`) when using `--target` platforms.
    /// go / cargo: ignored.
    #[arg(long = "node", value_name = "VERSION", action = clap::ArgAction::Append)]
    pub nodes: Vec<String>,

    /// Force re-download even if the package/module version is in the SQLite cache
    /// or listed in `--catalog`.
    #[arg(long = "force")]
    pub force: bool,

    /// Max concurrent toolchain workers. Default: CPU count (max 16)
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// SQLite DB that records completed downloads (default: under AK config dir)
    #[arg(long = "cache-db", value_name = "FILE")]
    pub cache_db: Option<PathBuf>,
}

impl DownloadArgs {
    pub async fn execute(self, global: &GlobalArgs) -> Result<()> {
        let cli_eco = if self.ecosystem.go {
            Some(Ecosystem::Go)
        } else if self.ecosystem.npm {
            Some(Ecosystem::Npm)
        } else if self.ecosystem.pypi {
            Some(Ecosystem::Pypi)
        } else if self.ecosystem.cargo {
            Some(Ecosystem::Cargo)
        } else {
            None
        };

        if let Some(eco) = cli_eco {
            let input = self.input.clone().ok_or_else(|| {
                AkError::ConfigError(
                    "CLI mode requires an input file (or use --config download.config)".into(),
                )
            })?;
            return run_cli_job(self, eco, input, global).await;
        }

        if self.input.is_some() {
            return Err(AkError::ConfigError(
                "Input without --go/--npm/--pypi/--cargo: put the plan in download.config, or pass an ecosystem flag"
                    .into(),
            )
            .into());
        }

        let config_path = self
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_NAME));
        if !config_path.is_file() {
            return Err(AkError::ConfigError(format!(
                "No ecosystem flag and config not found: {} (pass --config PATH or create {})",
                config_path.display(),
                DEFAULT_CONFIG_NAME
            ))
            .into());
        }

        run_config_plan(&config_path, &self, global).await
    }
}

async fn run_cli_job(
    args: DownloadArgs,
    eco: Ecosystem,
    input: PathBuf,
    global: &GlobalArgs,
) -> Result<()> {
    if args.config.is_some() {
        return Err(AkError::ConfigError(
            "Use either CLI flags (--go/--npm/--pypi/--cargo …) or --config, not both".into(),
        )
        .into());
    }
    if !input.is_file() {
        return Err(AkError::ConfigError(format!(
            "Input is not a file: {}",
            input.display()
        ))
        .into());
    }
    if matches!(eco, Ecosystem::Go | Ecosystem::Cargo) && !args.targets.is_empty() {
        return Err(AkError::ConfigError(
            "--target is only supported with --npm or --pypi".into(),
        )
        .into());
    }
    if matches!(eco, Ecosystem::Go | Ecosystem::Cargo) && !args.nodes.is_empty() {
        return Err(AkError::ConfigError(
            "--node is only supported with --npm or --pypi".into(),
        )
        .into());
    }

    let nodes = if args.nodes.is_empty() {
        Vec::new()
    } else if eco == Ecosystem::Npm {
        cache::parse_node_list(&args.nodes)?
    } else {
        // pypi: treat as Python versions (allow comma-separated)
        let mut out = Vec::new();
        for item in &args.nodes {
            for part in item.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    out.push(part.to_string());
                }
            }
        }
        out
    };

    let job = ResolvedJob {
        ecosystem: eco,
        input,
        output: args.output.clone(),
        output_dir: PathBuf::from("."),
        all_versions: args.all_versions,
        targets: args.targets.clone(),
        nodes,
        proxy: None,
        sumdb: None,
        registry: None,
        options: toml::Table::new(),
    };
    let jobs_n = parallel::resolve_jobs(args.jobs);
    let cache = open_cache(args.cache_db.as_deref(), args.force, &global.format)?;
    let catalog = load_catalog(args.catalog.as_deref(), &global.format)?;
    if !matches!(global.format, OutputFormat::Quiet) {
        eprintln!("parallel jobs: {jobs_n}");
    }
    run_jobs(
        &[job],
        args.work_dir.as_deref(),
        &cache,
        catalog.as_ref(),
        jobs_n,
        global,
    )
    .await
}

async fn run_config_plan(
    config_path: &Path,
    args: &DownloadArgs,
    global: &GlobalArgs,
) -> Result<()> {
    let cfg = DownloadConfigFile::load(config_path)?;
    let mut jobs = cfg.resolve_jobs(config_path)?;
    if jobs.is_empty() {
        return Err(AkError::ConfigError(format!(
            "{} contains no download jobs",
            config_path.display()
        ))
        .into());
    }

    // CLI can still override work_dir / force all_versions / targets / nodes for every job
    if args.all_versions {
        for j in &mut jobs {
            j.all_versions = true;
        }
    }
    if !args.targets.is_empty() {
        for j in &mut jobs {
            if matches!(j.ecosystem, Ecosystem::Npm | Ecosystem::Pypi) {
                j.targets = args.targets.clone();
            }
        }
    }
    if !args.nodes.is_empty() {
        for j in &mut jobs {
            match j.ecosystem {
                Ecosystem::Npm => {
                    j.nodes = cache::parse_node_list(&args.nodes)?;
                }
                Ecosystem::Pypi => {
                    let mut nodes = Vec::new();
                    for item in &args.nodes {
                        for part in item.split(',') {
                            let part = part.trim();
                            if !part.is_empty() {
                                nodes.push(part.to_string());
                            }
                        }
                    }
                    j.nodes = nodes;
                }
                _ => {}
            }
        }
    }

    let work_dir = args
        .work_dir
        .clone()
        .or_else(|| {
            cfg.work_dir.as_ref().map(|p| {
                let base = config_path.parent().unwrap_or_else(|| Path::new("."));
                if p.is_absolute() {
                    p.clone()
                } else {
                    base.join(p)
                }
            })
        });

    let force = args.force || cfg.force;
    let jobs_n = parallel::resolve_jobs(args.jobs.or(cfg.jobs));
    let cache_db = args.cache_db.clone().or_else(|| {
        cfg.cache_db.as_ref().map(|p| {
            let base = config_path.parent().unwrap_or_else(|| Path::new("."));
            if p.is_absolute() {
                p.clone()
            } else {
                base.join(p)
            }
        })
    });
    let cache = open_cache(cache_db.as_deref(), force, &global.format)?;
    let catalog_path = args.catalog.clone().or_else(|| {
        cfg.catalog.as_ref().map(|p| {
            let base = config_path.parent().unwrap_or_else(|| Path::new("."));
            if p.is_absolute() {
                p.clone()
            } else {
                base.join(p)
            }
        })
    });
    let catalog = load_catalog(catalog_path.as_deref(), &global.format)?;

    if !matches!(global.format, OutputFormat::Quiet) {
        eprintln!(
            "Loaded {} ({} job(s), jobs={})",
            config_path.display(),
            jobs.len(),
            jobs_n
        );
    }

    run_jobs(
        &jobs,
        work_dir.as_deref(),
        &cache,
        catalog.as_ref(),
        jobs_n,
        global,
    )
    .await
}

fn open_cache(
    explicit: Option<&Path>,
    force: bool,
    format: &OutputFormat,
) -> Result<Arc<DownloadCache>> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => default_cache_db_path()?,
    };
    if !matches!(format, OutputFormat::Quiet) {
        if force {
            eprintln!(
                "download cache: {} (--force, will re-fetch)",
                path.display()
            );
        } else {
            eprintln!("download cache: {}", path.display());
        }
    }
    Ok(Arc::new(DownloadCache::open(&path, force)?))
}

fn load_catalog(
    path: Option<&Path>,
    format: &OutputFormat,
) -> Result<Option<Arc<ServerCatalog>>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let cat = ServerCatalog::load(path)?;
    if !matches!(format, OutputFormat::Quiet) {
        eprintln!(
            "server catalog: {} ({} entries)",
            path.display(),
            cat.len()
        );
    }
    Ok(Some(Arc::new(cat)))
}

async fn run_jobs(
    jobs: &[ResolvedJob],
    work_dir: Option<&Path>,
    cache: &Arc<DownloadCache>,
    catalog: Option<&Arc<ServerCatalog>>,
    jobs_n: usize,
    global: &GlobalArgs,
) -> Result<()> {
    let keep_work = work_dir.is_some();
    let work = match work_dir {
        Some(d) => {
            std::fs::create_dir_all(d).map_err(|e| {
                AkError::ConfigError(format!("Cannot create work dir {}: {e}", d.display()))
            })?;
            WorkDir::Persistent(d.to_path_buf())
        }
        None => {
            let tmp = tempfile::tempdir()
                .map_err(|e| AkError::ConfigError(format!("Cannot create temp work dir: {e}")))?;
            WorkDir::Temp(tmp)
        }
    };

    for (i, job) in jobs.iter().enumerate() {
        if !matches!(global.format, OutputFormat::Quiet) {
            eprintln!(
                "=== job {}/{}: {:?} <- {} ===",
                i + 1,
                jobs.len(),
                job.ecosystem,
                job.input.display()
            );
        }
        if !job.options.is_empty() && !matches!(global.format, OutputFormat::Quiet) {
            eprintln!(
                "note: job has {} options key(s) reserved for future ecosystems",
                job.options.len()
            );
        }
        if !job.input.is_file() {
            return Err(AkError::ConfigError(format!(
                "Job input is not a file: {}",
                job.input.display()
            ))
            .into());
        }

        // Per-job subdirectory under shared work dir to avoid collisions
        let job_work = work.path().join(format!("job-{}", i + 1));
        std::fs::create_dir_all(&job_work).map_err(|e| {
            AkError::ConfigError(format!("Cannot create {}: {e}", job_work.display()))
        })?;

        let (output, auto_name) = naming::resolve_job_output(
            job.output.as_deref(),
            &job.output_dir,
            job.ecosystem,
            job.all_versions,
            &job.targets,
            &job.nodes,
            catalog.is_some(),
        );
        if auto_name && !matches!(global.format, OutputFormat::Quiet) {
            eprintln!("auto output: {}", output.display());
        }

        match job.ecosystem {
            Ecosystem::Go => {
                if !job.targets.is_empty() && !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("note: targets ignored for go job");
                }
                if !job.nodes.is_empty() && !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("note: nodes ignored for go job");
                }
                if job.registry.is_some() && !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("note: registry ignored for go job (use proxy / sumdb)");
                }
                let upstream = UpstreamConfig::from_job(job);
                if !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("GOPROXY={} GOSUMDB={}", upstream.goproxy(), upstream.gosumdb());
                }
                go::download_go(
                    &job.input,
                    &job_work,
                    &output,
                    job.all_versions,
                    &upstream,
                    Arc::clone(cache),
                    catalog.cloned(),
                    jobs_n,
                    &global.format,
                    auto_name,
                )
                .await?;
            }
            Ecosystem::Npm => {
                if job.proxy.is_some() || job.sumdb.is_some() {
                    if !matches!(global.format, OutputFormat::Quiet) {
                        eprintln!("note: proxy/sumdb ignored for npm job (use registry)");
                    }
                }
                let npm_targets = npm::parse_npm_targets(&job.targets)?;
                let nodes = if job.nodes.is_empty() {
                    Vec::new()
                } else {
                    cache::parse_node_list(&job.nodes)?
                };
                let upstream = UpstreamConfig::from_job(job);
                if !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!(
                        "npm registry={} (parallel installs: {jobs_n})",
                        upstream.npm_registry()
                    );
                }
                npm::download_npm(
                    &job.input,
                    &job_work,
                    &output,
                    job.all_versions,
                    &npm_targets,
                    &nodes,
                    &upstream,
                    Arc::clone(cache),
                    catalog.cloned(),
                    jobs_n,
                    &global.format,
                    auto_name,
                )
                .await?;
            }
            Ecosystem::Pypi => {
                if job.proxy.is_some() || job.sumdb.is_some() {
                    if !matches!(global.format, OutputFormat::Quiet) {
                        eprintln!("note: proxy/sumdb ignored for pypi job (use registry)");
                    }
                }
                let upstream = UpstreamConfig::from_job(job);
                if !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!(
                        "pypi index={} (parallel downloads: {jobs_n})",
                        upstream.pypi_index()
                    );
                }
                pypi::download_pypi(
                    &job.input,
                    &job_work,
                    &output,
                    job.all_versions,
                    &job.targets,
                    &job.nodes,
                    &upstream,
                    Arc::clone(cache),
                    catalog.cloned(),
                    jobs_n,
                    &global.format,
                    auto_name,
                )
                .await?;
            }
            Ecosystem::Cargo => {
                if !job.targets.is_empty() && !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("note: targets ignored for cargo job");
                }
                if !job.nodes.is_empty() && !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!("note: nodes ignored for cargo job");
                }
                if job.proxy.is_some() || job.sumdb.is_some() {
                    if !matches!(global.format, OutputFormat::Quiet) {
                        eprintln!("note: proxy/sumdb ignored for cargo job (use registry)");
                    }
                }
                let upstream = UpstreamConfig::from_job(job);
                if !matches!(global.format, OutputFormat::Quiet) {
                    eprintln!(
                        "cargo registry={} (parallel fetches: {jobs_n})",
                        upstream.cargo_sparse_registry()
                    );
                }
                cargo::download_cargo(
                    &job.input,
                    &job_work,
                    &output,
                    job.all_versions,
                    &upstream,
                    Arc::clone(cache),
                    catalog.cloned(),
                    jobs_n,
                    &global.format,
                    auto_name,
                )
                .await?;
            }
        }
    }

    if keep_work && !matches!(global.format, OutputFormat::Quiet) {
        eprintln!(
            "Note: --work-dir {} is only retained as an empty parent; per-job download trees are removed after each zip",
            work.path().display()
        );
    }
    Ok(())
}

enum WorkDir {
    Temp(tempfile::TempDir),
    Persistent(PathBuf),
}

impl WorkDir {
    fn path(&self) -> &Path {
        match self {
            Self::Temp(t) => t.path(),
            Self::Persistent(p) => p.as_path(),
        }
    }
}
