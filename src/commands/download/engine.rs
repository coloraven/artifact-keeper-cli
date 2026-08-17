//! Shared ferry download engine: list → (optional expand) → concurrent toolchain
//! fetch per root → soft-fail → manifest → zip → cleanup → error summary.
//!
//! Language-specific code implements [`LanguageToolchain`]; orchestration lives here
//! so npm / go / pypi / cargo share the same control flow.
//!
//! Toolchain downloads stay under the job work directory (never the user’s global
//! `~/go`, `~/.npm`, `~/.cargo`, pip cache, …). After the ferry zip is written,
//! that work tree is deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use miette::Result;

use super::cache::DownloadCache;
use super::catalog::ServerCatalog;
use super::config::UpstreamConfig;
use super::errors::{print_error_summary, UnitError};
use super::manifest::{file_entry, FerryManifest, ModuleEntry, RootSpec};
use super::pack::zip_dir;
use super::parallel;
use crate::error::AkError;
use crate::output::OutputFormat;

/// Shared paths for one ferry job.
#[derive(Debug, Clone)]
pub struct WorkDirs {
    pub work: PathBuf,
    pub payload: PathBuf,
    /// Ecosystem layout under payload (`npm/`, `download/`, `pypi/`, …)
    pub artifacts: PathBuf,
    /// Shared toolchain cache under work (`npm-cache/`, `gomodcache/`, …)
    pub tool_cache: PathBuf,
}

impl WorkDirs {
    pub fn create(work: &Path, artifact_subdir: &str, tool_cache_subdir: &str) -> Result<Self> {
        let payload = work.join("payload");
        let artifacts = payload.join(artifact_subdir);
        let tool_cache = work.join(tool_cache_subdir);
        let tmp = work.join("tmp");
        std::fs::create_dir_all(&payload)
            .map_err(|e| AkError::ConfigError(format!("mkdir payload: {e}")))?;
        std::fs::create_dir_all(&artifacts)
            .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", artifacts.display())))?;
        std::fs::create_dir_all(&tool_cache)
            .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", tool_cache.display())))?;
        std::fs::create_dir_all(&tmp)
            .map_err(|e| AkError::ConfigError(format!("mkdir tmp: {e}")))?;
        Ok(Self {
            work: work.to_path_buf(),
            payload,
            artifacts,
            tool_cache,
        })
    }

    /// Go build cache (separate from module cache).
    pub fn gocache(&self) -> PathBuf {
        self.work.join("gocache")
    }
}

/// Point temp dirs at the job work tree so toolchains do not write under the user profile.
pub fn apply_isolated_temp(cmd: &mut Command, work: &Path) {
    let tmp = work.join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    cmd.env("TMPDIR", &tmp);
    cmd.env("TMP", &tmp);
    cmd.env("TEMP", &tmp);
}

/// Remove the job work directory after the ferry zip is safely on disk.
pub fn cleanup_job_work(work: &Path, format: &OutputFormat) {
    if !work.exists() {
        return;
    }
    if !matches!(*format, OutputFormat::Quiet) {
        eprintln!("Cleaning download work dir {}…", work.display());
    }
    if let Err(e) = std::fs::remove_dir_all(work) {
        if !matches!(*format, OutputFormat::Quiet) {
            eprintln!(
                "warning: failed to remove work dir {}: {e}",
                work.display()
            );
        }
    }
}

/// One module/package version produced by a root fetch.
#[derive(Debug, Clone)]
pub struct FetchedModule {
    pub name: String,
    pub version: String,
    pub name_encoded: Option<String>,
    /// Absolute paths to include in the ferry zip
    pub files: Vec<PathBuf>,
}

/// Result of fetching one root (install / go get / pip download / …).
#[derive(Debug, Default)]
pub struct RootFetchResult {
    pub via: String,
    pub modules: Vec<FetchedModule>,
    /// Soft failures inside an otherwise successful root (e.g. one `npm pack` ETARGET).
    pub soft_errors: Vec<UnitError>,
}

/// Options shared by every language job.
pub struct FerryOpts {
    pub all_versions: bool,
    pub upstream: UpstreamConfig,
    pub cache: Arc<DownloadCache>,
    pub catalog: Option<Arc<ServerCatalog>>,
    pub jobs: usize,
    pub format: OutputFormat,
    /// When true, append `-r{n}-m{m}` to the zip filename after packing.
    pub auto_name: bool,
}

/// One scheduled fetch unit (usually one root; npm may expand to root×target×node).
#[derive(Debug, Clone)]
pub struct RootPass {
    pub pass_idx: usize,
    pub total: usize,
    pub root: RootSpec,
    /// Opaque language context (e.g. JSON `{"target":"linux-x64","node":"18"}`).
    pub context: String,
}

/// Per-language driver. Only ecosystem-specific steps live here.
pub trait LanguageToolchain: Send + Sync {
    fn ecosystem(&self) -> &'static str;

    /// Hint in the final `ak artifact push <…-repo>` message.
    fn push_repo_hint(&self) -> &'static str {
        self.ecosystem()
    }

    fn artifact_subdir(&self) -> &'static str;
    fn tool_cache_subdir(&self) -> &'static str;

    fn ensure_toolchain(&self) -> Result<()>;

    fn resolve_roots(&self, input: &Path) -> Result<Vec<RootSpec>>;

    /// Expand roots to all published versions. `work` is the isolated job dir for caches.
    fn expand_all_versions(
        &self,
        roots: Vec<RootSpec>,
        upstream: &UpstreamConfig,
        format: &OutputFormat,
        work: &Path,
    ) -> Result<Vec<RootSpec>>;

    /// Turn roots into concrete fetch passes. Default: one pass per root.
    fn expand_passes(&self, roots: Vec<RootSpec>) -> Vec<RootPass> {
        let total = roots.len();
        roots
            .into_iter()
            .enumerate()
            .map(|(i, root)| RootPass {
                pass_idx: i + 1,
                total,
                root,
                context: String::new(),
            })
            .collect()
    }

    /// If this pass is fully cached, restore into payload and return `true`.
    fn try_restore_cached(
        &self,
        pass: &RootPass,
        dirs: &WorkDirs,
        cache: &DownloadCache,
        known: &mut HashSet<(String, String)>,
        manifest: &mut FerryManifest,
        format: &OutputFormat,
    ) -> Result<bool>;

    /// Isolated fetch for one pass. Runs on a worker thread.
    fn fetch_one(
        &self,
        pass: RootPass,
        dirs: WorkDirs,
        upstream: UpstreamConfig,
        cache: Arc<DownloadCache>,
        quiet: bool,
    ) -> std::result::Result<RootFetchResult, UnitError>;

    /// Optional post-pass (e.g. Go full tree sync / backfill). Default: no-op.
    fn after_all_fetches(
        &self,
        _dirs: &WorkDirs,
        _manifest: &mut FerryManifest,
        _format: &OutputFormat,
    ) -> Result<()> {
        Ok(())
    }
}

/// Shared orchestration for every language.
pub async fn run_ferry(
    tool: Arc<dyn LanguageToolchain>,
    input: &Path,
    work: &Path,
    output: &Path,
    opts: FerryOpts,
) -> Result<()> {
    let mut roots = tool.resolve_roots(input)?;
    if roots.is_empty() {
        return Err(AkError::ConfigError(format!(
            "No {} packages/modules found in input",
            tool.ecosystem()
        ))
        .into());
    }

    tool.ensure_toolchain()?;

    // Create isolated work layout before any toolchain network calls so version
    // expansion and fetches never touch the user’s global caches.
    let dirs = WorkDirs::create(work, tool.artifact_subdir(), tool.tool_cache_subdir())?;

    if opts.all_versions {
        if !matches!(opts.format, OutputFormat::Quiet) {
            eprintln!(
                "Expanding {} root(s) to all published versions ({})…",
                roots.len(),
                tool.ecosystem()
            );
        }
        roots = tool.expand_all_versions(roots, &opts.upstream, &opts.format, work)?;
        if roots.is_empty() {
            cleanup_job_work(work, &opts.format);
            return Err(AkError::ConfigError(format!(
                "No published versions found for listed {} packages/modules",
                tool.ecosystem()
            ))
            .into());
        }
        if !matches!(opts.format, OutputFormat::Quiet) {
            eprintln!(
                "Will fetch {} {} version(s)",
                roots.len(),
                tool.ecosystem()
            );
        }
    }

    let mut manifest =
        FerryManifest::load_or_create(&dirs.payload, tool.ecosystem(), roots.clone())?;
    let mut known: HashSet<(String, String)> = manifest
        .modules
        .iter()
        .map(|m| (m.name.clone(), m.version.clone()))
        .collect();
    let mut unit_errors: Vec<UnitError> = Vec::new();

    let passes = tool.expand_passes(roots.clone());
    let mut pending: Vec<RootPass> = Vec::new();
    let force = opts.cache.force();
    for pass in passes {
        // --all-versions: each pass is one package; skip if already on server.
        if opts.all_versions {
            if let Some(cat) = &opts.catalog {
                if let Some(ver) = pass.root.version.as_deref().filter(|v| !v.is_empty()) {
                    if cat.should_skip(force, tool.ecosystem(), &pass.root.name, ver) {
                        if !matches!(opts.format, OutputFormat::Quiet) {
                            eprintln!(
                                "[{}/{}] skip (server catalog) {}@{ver}",
                                pass.pass_idx, pass.total, pass.root.name
                            );
                        }
                        continue;
                    }
                }
            }
        }

        match tool.try_restore_cached(
            &pass,
            &dirs,
            &opts.cache,
            &mut known,
            &mut manifest,
            &opts.format,
        ) {
            Ok(true) => continue,
            Ok(false) => pending.push(pass),
            Err(e) => {
                unit_errors.push(UnitError::new(
                    format!("restore cached {}", format_root(&pass.root)),
                    e.to_string(),
                ));
            }
        }
    }

    if !matches!(opts.format, OutputFormat::Quiet) && !pending.is_empty() {
        eprintln!(
            "Running {} {} fetch(es) with up to {} concurrent worker(s)",
            pending.len(),
            tool.ecosystem(),
            opts.jobs
        );
    }

    let quiet = matches!(opts.format, OutputFormat::Quiet);
    let dirs_c = dirs.clone();
    let upstream = opts.upstream.clone();
    let cache = Arc::clone(&opts.cache);
    let tool_c = Arc::clone(&tool);

    let (outcomes, fetch_errs) =
        parallel::run_blocking_jobs_soft(opts.jobs, pending, move |pass| {
            tool_c.fetch_one(
                pass,
                dirs_c.clone(),
                upstream.clone(),
                Arc::clone(&cache),
                quiet,
            )
        })
        .await?;
    unit_errors.extend(fetch_errs);

    for outcome in outcomes {
        unit_errors.extend(outcome.soft_errors);
        for m in outcome.modules {
            if let Some(cat) = &opts.catalog {
                if cat.should_skip(force, tool.ecosystem(), &m.name, &m.version) {
                    if !matches!(opts.format, OutputFormat::Quiet) {
                        eprintln!(
                            "  skip (server catalog) {}@{}",
                            m.name, m.version
                        );
                    }
                    // Remember so we don't re-add via another root's closure.
                    known.insert((m.name.clone(), m.version.clone()));
                    continue;
                }
            }
            if !known.insert((m.name.clone(), m.version.clone())) {
                continue;
            }
            let files = match m
                .files
                .iter()
                .map(|p| file_entry(&dirs.payload, p))
                .collect::<Result<Vec<_>>>()
            {
                Ok(f) => f,
                Err(e) => {
                    unit_errors.push(UnitError::new(
                        format!("hash {}@{}", m.name, m.version),
                        e.to_string(),
                    ));
                    continue;
                }
            };
            if let Err(e) = manifest.record_module(
                &dirs.payload,
                ModuleEntry {
                    name: m.name.clone(),
                    name_encoded: m.name_encoded,
                    version: m.version.clone(),
                    files,
                    via: outcome.via.clone(),
                },
            ) {
                unit_errors.push(UnitError::new(
                    format!("manifest {}@{}", m.name, m.version),
                    e.to_string(),
                ));
            }
        }
    }

    if let Err(e) = tool.after_all_fetches(&dirs, &mut manifest, &opts.format) {
        unit_errors.push(UnitError::new(
            format!("{} post-process", tool.ecosystem()),
            e.to_string(),
        ));
    }

    finish_ferry(
        tool.ecosystem(),
        tool.push_repo_hint(),
        &dirs.payload,
        output,
        work,
        roots.len(),
        manifest.modules.len(),
        &unit_errors,
        &opts.format,
        opts.auto_name,
    )
}

/// Zip payload, delete the job work tree, then report soft failures.
pub fn finish_ferry(
    ecosystem: &str,
    push_repo_hint: &str,
    payload: &Path,
    output: &Path,
    work: &Path,
    roots_len: usize,
    modules_len: usize,
    unit_errors: &[UnitError],
    format: &OutputFormat,
    auto_name: bool,
) -> Result<()> {
    let final_output = if auto_name {
        super::naming::append_stats(output, roots_len, modules_len)
    } else {
        output.to_path_buf()
    };

    if !matches!(*format, OutputFormat::Quiet) {
        eprintln!(
            "Packing {} {} version(s) -> {}",
            modules_len,
            ecosystem,
            final_output.display()
        );
    }
    zip_dir(payload, &final_output)?;
    // Zip is on disk — drop payload + toolchain caches so the host env stays clean.
    cleanup_job_work(work, format);

    if matches!(*format, OutputFormat::Quiet) {
        println!("{}", final_output.display());
    } else {
        eprintln!(
            "Wrote {} ({} roots, {} packages). Upload with:\n  ak artifact push <{push_repo_hint}-repo> --from-archive {}",
            final_output.display(),
            roots_len,
            modules_len,
            final_output.display()
        );
    }

    if print_error_summary(ecosystem, unit_errors) {
        return Err(AkError::ConfigError(format!(
            "{} {ecosystem} unit(s) failed (details above); successful packages are in the ferry zip",
            unit_errors.len()
        ))
        .into());
    }
    Ok(())
}

pub fn format_root(root: &RootSpec) -> String {
    match &root.version {
        Some(v) => format!("{}@{v}", root.name),
        None => root.name.clone(),
    }
}

pub fn trim_err(s: &str) -> String {
    let s = s.trim();
    if s.len() > 1200 {
        format!("{}…", &s[..1200])
    } else {
        s.to_string()
    }
}
