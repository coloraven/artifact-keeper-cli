//! npm ferry download: isolated `npm install` per root, then `npm pack` each resolved pkg.
//! Orchestration lives in [`super::engine`]; this module is the npm [`LanguageToolchain`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use miette::Result;
use serde_json::Value;

use super::cache::{CacheKey, DownloadCache};
use super::config::UpstreamConfig;
use super::engine::{self, format_root, trim_err, FetchedModule, RootFetchResult, RootPass, WorkDirs};
use super::errors::UnitError;
use super::manifest::{file_entry, parse_module_list, FerryManifest, ModuleEntry, RootSpec};
use crate::error::AkError;
use crate::output::OutputFormat;

/// On Windows, npm is `npm.cmd` (not `npm.exe`). `Command::new("npm")` uses
/// CreateProcess and will not resolve PATHEXT / shell scripts the way PowerShell does.
fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

fn npm_cmd() -> Command {
    Command::new(npm_program())
}

/// A single npm install target (`--os` / `--cpu` / optional `--libc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmTarget {
    pub os: String,
    pub cpu: String,
    pub libc: Option<String>,
    /// Original CLI token, e.g. `linux-arm64-musl`
    pub label: String,
}

impl NpmTarget {
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(AkError::ConfigError("Empty --target".into()).into());
        }
        let parts: Vec<&str> = s.split('-').collect();
        // OS may contain no dashes; CPU is next; optional libc after.
        // Special case: win32-x64, darwin-arm64, linux-x64, linux-arm64-musl
        match parts.as_slice() {
            [os, cpu] => Ok(Self {
                os: (*os).to_string(),
                cpu: (*cpu).to_string(),
                libc: None,
                label: s.to_string(),
            }),
            [os, cpu, libc] => Ok(Self {
                os: (*os).to_string(),
                cpu: (*cpu).to_string(),
                libc: Some((*libc).to_string()),
                label: s.to_string(),
            }),
            _ => Err(AkError::ConfigError(format!(
                "Invalid --target '{s}' (expected OS-CPU or OS-CPU-LIBC, e.g. linux-x64, linux-arm64-musl, darwin-arm64, win32-x64)"
            ))
            .into()),
        }
    }
}

pub fn parse_npm_targets(raw: &[String]) -> Result<Vec<NpmTarget>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in raw {
        // Allow comma-separated in one flag: --target linux-x64,darwin-arm64
        for part in item.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let t = NpmTarget::parse(part)?;
            if seen.insert(t.label.clone()) {
                out.push(t);
            }
        }
    }
    Ok(out)
}

pub async fn download_npm(
    input: &Path,
    work: &Path,
    output: &Path,
    all_versions: bool,
    targets: &[NpmTarget],
    nodes: &[String],
    upstream: &UpstreamConfig,
    cache: Arc<DownloadCache>,
    catalog: Option<Arc<super::catalog::ServerCatalog>>,
    jobs: usize,
    format: &OutputFormat,
    auto_name: bool,
) -> Result<()> {
    if all_versions {
        if (!targets.is_empty() || !nodes.is_empty()) && !matches!(format, OutputFormat::Quiet) {
            eprintln!(
                "note: --target/--node are ignored with --all-versions (packs root tarballs only); \
                 list platform/ABI packages explicitly if needed"
            );
        }
    } else if (!targets.is_empty() || !nodes.is_empty()) && !matches!(format, OutputFormat::Quiet)
    {
        let labels: Vec<_> = targets.iter().map(|t| t.label.as_str()).collect();
        let node_labels = if nodes.is_empty() {
            "host".to_string()
        } else {
            nodes.join(",")
        };
        eprintln!(
            "npm platform targets: {}; node: {}",
            if labels.is_empty() {
                "host".into()
            } else {
                labels.join(",")
            },
            node_labels
        );
    }

    let tool = Arc::new(NpmToolchain {
        targets: targets.to_vec(),
        nodes: nodes.to_vec(),
        all_versions,
    });
    engine::run_ferry(
        tool,
        input,
        work,
        output,
        engine::FerryOpts {
            all_versions,
            upstream: upstream.clone(),
            cache,
            catalog,
            jobs,
            format: format.clone(),
            auto_name,
        },
    )
    .await
}

struct NpmToolchain {
    targets: Vec<NpmTarget>,
    nodes: Vec<String>,
    all_versions: bool,
}

#[derive(Debug, Clone, Default)]
struct NpmCtx {
    /// `--all-versions`: pack root tarball only (no install / dep tree).
    pack_only: bool,
    target: Option<NpmTarget>,
    node: Option<String>,
}

fn encode_npm_ctx(ctx: &NpmCtx) -> String {
    serde_json::json!({
        "pack_only": ctx.pack_only,
        "target": ctx.target.as_ref().map(|t| &t.label),
        "node": ctx.node.as_deref(),
    })
    .to_string()
}

fn decode_npm_ctx(raw: &str, targets: &[NpmTarget]) -> NpmCtx {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return NpmCtx::default();
    };
    let pack_only = v
        .get("pack_only")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let target = v
        .get("target")
        .and_then(|x| x.as_str())
        .and_then(|label| targets.iter().find(|t| t.label == label).cloned());
    let node = v
        .get("node")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    NpmCtx {
        pack_only,
        target,
        node,
    }
}

impl engine::LanguageToolchain for NpmToolchain {
    fn ecosystem(&self) -> &'static str {
        "npm"
    }
    fn artifact_subdir(&self) -> &'static str {
        "npm"
    }
    fn tool_cache_subdir(&self) -> &'static str {
        "npm-cache"
    }
    fn ensure_toolchain(&self) -> Result<()> {
        ensure_npm_toolchain()
    }
    fn resolve_roots(&self, input: &Path) -> Result<Vec<RootSpec>> {
        resolve_npm_roots(input)
    }
    fn expand_all_versions(
        &self,
        roots: Vec<RootSpec>,
        upstream: &UpstreamConfig,
        format: &OutputFormat,
        work: &Path,
    ) -> Result<Vec<RootSpec>> {
        expand_npm_all_versions(&roots, upstream, format, work)
    }
    fn expand_passes(&self, roots: Vec<RootSpec>) -> Vec<RootPass> {
        if self.all_versions {
            let total = roots.len();
            return roots
                .into_iter()
                .enumerate()
                .map(|(i, root)| RootPass {
                    pass_idx: i + 1,
                    total,
                    root,
                    context: encode_npm_ctx(&NpmCtx {
                        pack_only: true,
                        ..Default::default()
                    }),
                })
                .collect();
        }

        let target_passes: Vec<Option<NpmTarget>> = if self.targets.is_empty() {
            vec![None]
        } else {
            self.targets.iter().cloned().map(Some).collect()
        };
        let node_passes: Vec<Option<String>> = if self.nodes.is_empty() {
            vec![None]
        } else {
            self.nodes.iter().cloned().map(Some).collect()
        };
        let total = roots.len() * target_passes.len() * node_passes.len();
        let mut out = Vec::with_capacity(total);
        let mut pass_idx = 0usize;
        for root in roots {
            for target in &target_passes {
                for node in &node_passes {
                    pass_idx += 1;
                    out.push(RootPass {
                        pass_idx,
                        total,
                        root: root.clone(),
                        context: encode_npm_ctx(&NpmCtx {
                            pack_only: false,
                            target: target.clone(),
                            node: node.clone(),
                        }),
                    });
                }
            }
        }
        out
    }
    fn try_restore_cached(
        &self,
        pass: &RootPass,
        dirs: &WorkDirs,
        cache: &DownloadCache,
        known: &mut HashSet<(String, String)>,
        manifest: &mut FerryManifest,
        format: &OutputFormat,
    ) -> Result<bool> {
        let ctx = decode_npm_ctx(&pass.context, &self.targets);
        let root = &pass.root;
        let spec = format_root(root);

        if ctx.pack_only {
            let Some(ver) = root.version.as_deref().filter(|v| !v.is_empty()) else {
                return Ok(false);
            };
            if known.contains(&(root.name.clone(), ver.to_string())) {
                return Ok(true);
            }
            let key = CacheKey::npm(&root.name, ver, None, None);
            if !cache.should_skip(&key)? {
                return Ok(false);
            }
            if !matches!(*format, OutputFormat::Quiet) {
                eprintln!(
                    "[{}/{}] skip cached {spec}",
                    pass.pass_idx, pass.total
                );
            }
            let dest = dirs.artifacts.join(sanitize_name(&root.name));
            let restored = cache.materialize(&key, &dest)?;
            if !known.insert((root.name.clone(), ver.to_string())) {
                return Ok(true);
            }
            let files = restored
                .iter()
                .map(|p| file_entry(&dirs.payload, p))
                .collect::<Result<Vec<_>>>()?;
            manifest.record_module(
                &dirs.payload,
                ModuleEntry {
                    name: root.name.clone(),
                    name_encoded: None,
                    version: ver.to_string(),
                    files,
                    via: format!("{} (all-versions)", root.name),
                },
            )?;
            return Ok(true);
        }

        let Some(ver) = root.version.as_deref().filter(|v| is_exact_npm_version(v)) else {
            return Ok(false);
        };
        let target_disp = ctx
            .target
            .as_ref()
            .map(|t| t.label.as_str())
            .unwrap_or("host");
        let node_disp = ctx.node.as_deref().unwrap_or("host");
        let root_key = CacheKey::npm(
            &root.name,
            ver,
            ctx.target.as_ref().map(|t| t.label.as_str()),
            ctx.node.as_deref(),
        );
        if !cache.should_skip(&root_key)? {
            return Ok(false);
        }
        if !matches!(*format, OutputFormat::Quiet) {
            eprintln!(
                "[{}/{}] skip cached install {spec} (target={target_disp}, node={node_disp})",
                pass.pass_idx, pass.total
            );
        }
        restore_npm_closure(
            cache,
            &root_key,
            &dirs.artifacts,
            &dirs.payload,
            known,
            manifest,
            &format!("{spec} [{target_disp}/node-{node_disp}] (cached)"),
        )?;
        Ok(true)
    }
    fn fetch_one(
        &self,
        pass: RootPass,
        dirs: WorkDirs,
        upstream: UpstreamConfig,
        cache: Arc<DownloadCache>,
        quiet: bool,
    ) -> std::result::Result<RootFetchResult, UnitError> {
        let ctx = decode_npm_ctx(&pass.context, &self.targets);
        if ctx.pack_only {
            return fetch_pack_only(pass, &dirs, &upstream, &cache, quiet);
        }
        fetch_install(pass, ctx, &dirs, &upstream, &cache, quiet)
    }
}

fn fetch_pack_only(
    pass: RootPass,
    dirs: &WorkDirs,
    upstream: &UpstreamConfig,
    cache: &DownloadCache,
    quiet: bool,
) -> std::result::Result<RootFetchResult, UnitError> {
    let root = &pass.root;
    let version = root.version.as_deref().unwrap_or("");
    if version.is_empty() {
        return Err(UnitError::new(
            format_root(root),
            "missing version for all-versions pack",
        ));
    }
    let spec = format!("{}@{version}", root.name);
    if !quiet {
        eprintln!("[{}/{}] npm pack {spec}", pass.pass_idx, pass.total);
    }
    let pack_cwd = dirs.work.join("pack-cwd");
    std::fs::create_dir_all(&pack_cwd).map_err(|e| {
        UnitError::new(format!("npm pack {spec}"), format!("mkdir pack-cwd: {e}"))
    })?;
    let tgz = pack_one(
        &pack_cwd,
        &dirs.tool_cache,
        &dirs.work,
        &dirs.artifacts,
        &root.name,
        version,
        upstream,
    )
    .map_err(|e| UnitError::new(format!("npm pack {spec}"), trim_err(&e.to_string())))?;
    let key = CacheKey::npm(&root.name, version, None, None);
    cache
        .store(&key, &[tgz.clone()])
        .map_err(|e| UnitError::new(format!("cache store {spec}"), e.to_string()))?;
    Ok(RootFetchResult {
        via: format!("{} (all-versions)", root.name),
        modules: vec![FetchedModule {
            name: root.name.clone(),
            version: version.to_string(),
            name_encoded: None,
            files: vec![tgz],
        }],
        soft_errors: Vec::new(),
    })
}

fn fetch_install(
    pass: RootPass,
    ctx: NpmCtx,
    dirs: &WorkDirs,
    upstream: &UpstreamConfig,
    cache: &DownloadCache,
    quiet: bool,
) -> std::result::Result<RootFetchResult, UnitError> {
    let spec = format_root(&pass.root);
    let target_disp = ctx
        .target
        .as_ref()
        .map(|t| t.label.as_str())
        .unwrap_or("host");
    let node_disp = ctx.node.as_deref().unwrap_or("host");
    if !quiet {
        eprintln!(
            "[{}/{}] npm install {spec} (isolated, target={target_disp}, node={node_disp})",
            pass.pass_idx, pass.total
        );
    }

    let isolate = dirs.work.join(format!("isolate-{}", pass.pass_idx));
    if isolate.exists() {
        std::fs::remove_dir_all(&isolate).ok();
    }
    std::fs::create_dir_all(&isolate).map_err(|e| {
        UnitError::new(format!("npm install {spec}"), format!("mkdir isolate: {e}"))
    })?;

    let pkg_json = serde_json::json!({
        "name": "ak-ferry-isolate",
        "private": true,
        "dependencies": {
            pass.root.name.clone(): pass.root.version.clone().unwrap_or_else(|| "*".into())
        }
    });
    std::fs::write(
        isolate.join("package.json"),
        serde_json::to_vec_pretty(&pkg_json).unwrap(),
    )
    .map_err(|e| {
        UnitError::new(
            format!("npm install {spec}"),
            format!("write package.json: {e}"),
        )
    })?;

    run_npm_install(
        &isolate,
        &dirs.tool_cache,
        &dirs.work,
        ctx.target.as_ref(),
        ctx.node.as_deref(),
        upstream,
    )
    .map_err(|e| UnitError::new(format!("npm install {spec}"), trim_err(&e.to_string())))?;

    let resolved = collect_installed_packages(&isolate, &dirs.work, &dirs.tool_cache).map_err(|e| {
        UnitError::new(format!("npm ls after {spec}"), trim_err(&e.to_string()))
    })?;
    let via = format!("{spec} [{target_disp}/node-{node_disp}]");
    let root_ver = resolved
        .iter()
        .find(|(n, _)| n == &pass.root.name)
        .map(|(_, v)| v.clone())
        .or_else(|| pass.root.version.clone())
        .unwrap_or_default();

    let pack_dir = isolate.join("ak-pack");
    std::fs::create_dir_all(&pack_dir).map_err(|e| {
        UnitError::new(format!("npm install {spec}"), format!("mkdir pack dir: {e}"))
    })?;

    let target_label = ctx.target.as_ref().map(|t| t.label.as_str());
    let node_ref = ctx.node.as_deref();
    let mut modules = Vec::with_capacity(resolved.len());
    let mut soft_errors = Vec::new();
    let mut closure_deps = Vec::new();

    for (name, version) in &resolved {
        let key = CacheKey::npm(name, version, target_label, node_ref);
        closure_deps.push((name.clone(), version.clone()));
        if cache.should_skip(&key).unwrap_or(false) {
            if !quiet {
                eprintln!("  skip cached pack {name}@{version}");
            }
            let dest = dirs.artifacts.join(sanitize_name(name));
            match cache.materialize(&key, &dest) {
                Ok(restored) if !restored.is_empty() => {
                    modules.push(FetchedModule {
                        name: name.clone(),
                        version: version.clone(),
                        name_encoded: None,
                        files: restored,
                    });
                }
                Ok(_) => soft_errors.push(UnitError::new(
                    format!("cache restore {name}@{version}"),
                    "empty cache restore",
                )),
                Err(e) => soft_errors.push(UnitError::new(
                    format!("cache restore {name}@{version}"),
                    trim_err(&e.to_string()),
                )),
            }
            continue;
        }
        match pack_one(
            &isolate,
            &dirs.tool_cache,
            &dirs.work,
            &pack_dir,
            name,
            version,
            upstream,
        ) {
            Ok(tgz) => {
                let dest_dir = dirs.artifacts.join(sanitize_name(name));
                if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                    soft_errors.push(UnitError::new(
                        format!("mkdir {}", dest_dir.display()),
                        e.to_string(),
                    ));
                    continue;
                }
                let Some(fname) = tgz.file_name() else {
                    soft_errors.push(UnitError::new(
                        format!("npm pack {name}@{version}"),
                        "pack missing filename",
                    ));
                    continue;
                };
                let dest = dest_dir.join(fname);
                let final_tgz = if tgz != dest {
                    if let Err(e) = std::fs::copy(&tgz, &dest) {
                        soft_errors.push(UnitError::new(
                            format!("copy {name}@{version}"),
                            e.to_string(),
                        ));
                        continue;
                    }
                    dest
                } else {
                    tgz
                };
                if let Err(e) = cache.store(&key, &[final_tgz.clone()]) {
                    soft_errors.push(UnitError::new(
                        format!("cache store {name}@{version}"),
                        trim_err(&e.to_string()),
                    ));
                    continue;
                }
                modules.push(FetchedModule {
                    name: name.clone(),
                    version: version.clone(),
                    name_encoded: None,
                    files: vec![final_tgz],
                });
            }
            Err(e) => {
                if !quiet {
                    eprintln!("  skip failed pack {name}@{version}");
                }
                soft_errors.push(UnitError::new(
                    format!("npm pack {name}@{version}"),
                    trim_err(&e.to_string()),
                ));
            }
        }
    }

    if !root_ver.is_empty() {
        let root_key = CacheKey::npm(&pass.root.name, &root_ver, target_label, node_ref);
        let _ = cache.store_closure(&root_key, &closure_deps);
    }

    Ok(RootFetchResult {
        via,
        modules,
        soft_errors,
    })
}

fn ensure_npm_toolchain() -> Result<()> {
    let status = npm_cmd()
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(AkError::ConfigError(
            "`npm` not found on PATH; install Node.js/npm to use `ak download --npm`".into(),
        )
        .into()),
    }
}

fn run_npm_install(
    isolate: &Path,
    npm_cache: &Path,
    work: &Path,
    target: Option<&NpmTarget>,
    node: Option<&str>,
    upstream: &UpstreamConfig,
) -> Result<()> {
    let mut cmd = npm_cmd();
    cmd.current_dir(isolate)
        .args([
            "install",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--cache",
        ])
        .arg(npm_cache)
        .env("npm_config_cache", npm_cache)
        .env("npm_config_update_notifier", "false");
    engine::apply_isolated_temp(&mut cmd, work);

    if let Some(t) = target {
        cmd.arg("--os").arg(&t.os);
        cmd.arg("--cpu").arg(&t.cpu);
        if let Some(libc) = &t.libc {
            cmd.arg("--libc").arg(libc);
        }
        cmd.env("npm_config_os", &t.os);
        cmd.env("npm_config_cpu", &t.cpu);
        if let Some(libc) = &t.libc {
            cmd.env("npm_config_libc", libc);
        }
    }

    if let Some(n) = node {
        let normalized = super::cache::normalize_node_target(n);
        cmd.arg("--target").arg(&normalized);
        cmd.env("npm_config_target", &normalized);
        cmd.env("npm_config_runtime", "node");
    }

    cmd.arg("--registry").arg(upstream.npm_registry());

    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn npm: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut hint = String::new();
        if let Some(t) = target {
            hint.push_str(&format!(" target={}", t.label));
        }
        if let Some(n) = node {
            hint.push_str(&format!(" node={n}"));
        }
        return Err(AkError::ConfigError(format!(
            "npm install{hint} failed:\n{stderr}"
        ))
        .into());
    }
    Ok(())
}

fn pack_one(
    isolate: &Path,
    npm_cache: &Path,
    work: &Path,
    packages_dir: &Path,
    name: &str,
    version: &str,
    upstream: &UpstreamConfig,
) -> Result<PathBuf> {
    let dest = packages_dir.join(sanitize_name(name));
    std::fs::create_dir_all(&dest)
        .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", dest.display())))?;

    let mut cmd = npm_cmd();
    cmd.current_dir(isolate)
        .args(["pack", &format!("{name}@{version}"), "--pack-destination"])
        .arg(&dest)
        .arg("--cache")
        .arg(npm_cache)
        .arg("--ignore-scripts")
        .env("npm_config_cache", npm_cache)
        .env("npm_config_update_notifier", "false");
    engine::apply_isolated_temp(&mut cmd, work);

    cmd.arg("--registry").arg(upstream.npm_registry());

    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn npm pack: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "npm pack {name}@{version} failed:\n{stderr}"
        ))
        .into());
    }

    // npm pack prints the filename on stdout
    let printed = String::from_utf8_lossy(&output.stdout);
    let fname = printed
        .lines()
        .rev()
        .find(|l| l.ends_with(".tgz"))
        .unwrap_or("")
        .trim();
    if !fname.is_empty() {
        let p = dest.join(fname);
        if p.is_file() {
            return Ok(p);
        }
    }

    // Fallback: any new tgz in dest
    let mut tgzs: Vec<_> = std::fs::read_dir(&dest)
        .map_err(|e| AkError::ConfigError(format!("read {}: {e}", dest.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("tgz"))
        .collect();
    tgzs.sort();
    tgzs.pop().ok_or_else(|| {
        AkError::ConfigError(format!("npm pack produced no tgz for {name}@{version}")).into()
    })
}

fn sanitize_name(name: &str) -> String {
    name.trim_start_matches('@').replace('/', "-")
}

fn is_exact_npm_version(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty()
        && !v.contains(['^', '~', '*', '>', '<', '|', ' ', 'x', 'X'])
        && !v.eq_ignore_ascii_case("latest")
}

fn restore_npm_closure(
    cache: &DownloadCache,
    root_key: &CacheKey,
    packages_dir: &Path,
    payload: &Path,
    known: &mut HashSet<(String, String)>,
    manifest: &mut FerryManifest,
    via: &str,
) -> Result<()> {
    for (name, version) in cache.closure_members(root_key)? {
        if !known.insert((name.clone(), version.clone())) {
            continue;
        }
        let key = CacheKey::npm(
            &name,
            &version,
            if root_key.target.is_empty() {
                None
            } else {
                Some(root_key.target.as_str())
            },
            if root_key.node.is_empty() {
                None
            } else {
                Some(root_key.node.as_str())
            },
        );
        let dest = packages_dir.join(sanitize_name(&name));
        let restored = cache.materialize(&key, &dest)?;
        let files = restored
            .iter()
            .map(|p| file_entry(payload, p))
            .collect::<Result<Vec<_>>>()?;
        manifest.record_module(
            payload,
            ModuleEntry {
                name,
                name_encoded: None,
                version,
                files,
                via: via.to_string(),
            },
        )?;
    }
    Ok(())
}

fn collect_installed_packages(isolate: &Path, work: &Path, npm_cache: &Path) -> Result<Vec<(String, String)>> {
    let mut cmd = npm_cmd();
    cmd.current_dir(isolate)
        .args(["ls", "--all", "--json", "--omit=dev"])
        .arg("--cache")
        .arg(npm_cache)
        .env("npm_config_cache", npm_cache)
        .env("npm_config_update_notifier", "false");
    engine::apply_isolated_temp(&mut cmd, work);
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("npm ls failed to spawn: {e}")))?;
    // npm ls may exit non-zero on peer issues; still parse stdout
    let v: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let mut out = Vec::new();
    walk_npm_ls(&v, &mut out);
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_npm_ls(node: &Value, out: &mut Vec<(String, String)>) {
    if let Some(deps) = node.get("dependencies").and_then(|d| d.as_object()) {
        for (logical_name, meta) in deps {
            if let Some(ver) = meta.get("version").and_then(|v| v.as_str()) {
                // Prefer registry identity from `resolved` so npm aliases
                // (e.g. string-width-cjs@4.2.3 → string-width@4.2.3) pack correctly.
                let (pack_name, pack_ver) = meta
                    .get("resolved")
                    .and_then(|r| r.as_str())
                    .and_then(parse_npm_tarball_resolved)
                    .unwrap_or_else(|| (logical_name.clone(), ver.to_string()));
                out.push((pack_name, pack_ver));
            }
            walk_npm_ls(meta, out);
        }
    }
}

/// Parse `https://registry…/string-width/-/string-width-4.2.3.tgz`
/// or scoped `…/@scope/pkg/-/pkg-1.0.0.tgz` → (name, version).
fn parse_npm_tarball_resolved(url: &str) -> Option<(String, String)> {
    let url = url.split(['?', '#']).next()?;
    let (_, path) = url.split_once("://")?;
    let path = path.split_once('/')?.1; // drop host
    let (name_part, file_part) = path.split_once("/-/")?;
    let name = percent_decode_npm(name_part);
    let file = file_part.strip_suffix(".tgz")?;
    let unscoped = name.rsplit('/').next()?;
    let version = file.strip_prefix(&format!("{unscoped}-"))?;
    if version.is_empty() {
        return None;
    }
    Some((name, version.to_string()))
}

fn percent_decode_npm(s: &str) -> String {
    // npm rarely percent-encodes in resolved paths; handle `%2F` for scoped names.
    s.replace("%2F", "/").replace("%2f", "/")
}

/// Deduplicate by package name, then expand each to every published version.
fn expand_npm_all_versions(
    roots: &[RootSpec],
    upstream: &UpstreamConfig,
    format: &OutputFormat,
    work: &Path,
) -> Result<Vec<RootSpec>> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if seen.insert(root.name.clone()) {
            names.push(root.name.clone());
        }
    }

    let npm_cache = work.join("npm-cache");
    std::fs::create_dir_all(&npm_cache)
        .map_err(|e| AkError::ConfigError(format!("mkdir npm-cache: {e}")))?;

    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if !matches!(format, OutputFormat::Quiet) {
            eprintln!("  [{}/{}] npm view {name} versions", i + 1, names.len());
        }
        let versions = npm_list_versions(name, upstream, work, &npm_cache)?;
        if versions.is_empty() {
            return Err(AkError::ConfigError(format!(
                "npm view {name} versions returned no versions"
            ))
            .into());
        }
        if !matches!(format, OutputFormat::Quiet) {
            eprintln!("    -> {} version(s)", versions.len());
        }
        for ver in versions {
            out.push(RootSpec {
                name: name.clone(),
                version: Some(ver),
            });
        }
    }
    Ok(out)
}

fn npm_list_versions(
    name: &str,
    upstream: &UpstreamConfig,
    work: &Path,
    npm_cache: &Path,
) -> Result<Vec<String>> {
    let mut cmd = npm_cmd();
    cmd.args(["view", name, "versions", "--json"])
        .arg("--cache")
        .arg(npm_cache)
        .arg("--registry")
        .arg(upstream.npm_registry())
        .env("npm_config_cache", npm_cache)
        .env("npm_config_update_notifier", "false");
    engine::apply_isolated_temp(&mut cmd, work);
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn npm view: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "npm view {name} versions failed:\n{stderr}"
        ))
        .into());
    }

    let v: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        AkError::ConfigError(format!("Invalid npm view versions JSON for {name}: {e}"))
    })?;

    // Single-version packages may return a bare string instead of an array.
    match v {
        Value::Array(arr) => Ok(arr
            .into_iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect()),
        Value::String(s) => Ok(vec![s]),
        _ => Err(AkError::ConfigError(format!(
            "Unexpected npm view versions shape for {name}: {v}"
        ))
        .into()),
    }
}

fn resolve_npm_roots(input: &Path) -> Result<Vec<RootSpec>> {
    let text = std::fs::read_to_string(input)
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", input.display())))?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "package.json" || text.trim_start().starts_with('{') {
        parse_package_json_deps(&text)
    } else {
        Ok(parse_module_list(&text))
    }
}

fn parse_package_json_deps(text: &str) -> Result<Vec<RootSpec>> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| AkError::ConfigError(format!("Invalid package.json: {e}")))?;
    let mut out = Vec::new();
    for key in ["dependencies", "optionalDependencies"] {
        if let Some(obj) = v.get(key).and_then(|x| x.as_object()) {
            for (name, ver) in obj {
                let version = ver.as_str().map(|s| s.to_string());
                out.push(RootSpec {
                    name: name.clone(),
                    version,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_package_json() {
        let text = r#"{
          "dependencies": { "lodash": "4.17.21" },
          "devDependencies": { "jest": "29.0.0" },
          "optionalDependencies": { "fsevents": "2.3.2" }
        }"#;
        let roots = parse_package_json_deps(text).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|r| r.name == "lodash"));
        assert!(roots.iter().any(|r| r.name == "fsevents"));
    }

    #[test]
    fn parse_target_variants() {
        let t = NpmTarget::parse("linux-x64").unwrap();
        assert_eq!(t.os, "linux");
        assert_eq!(t.cpu, "x64");
        assert!(t.libc.is_none());

        let t = NpmTarget::parse("linux-arm64-musl").unwrap();
        assert_eq!(t.os, "linux");
        assert_eq!(t.cpu, "arm64");
        assert_eq!(t.libc.as_deref(), Some("musl"));

        let t = NpmTarget::parse("darwin-arm64").unwrap();
        assert_eq!(t.os, "darwin");
        assert_eq!(t.cpu, "arm64");

        let t = NpmTarget::parse("win32-x64").unwrap();
        assert_eq!(t.os, "win32");
        assert_eq!(t.cpu, "x64");

        assert!(NpmTarget::parse("linux").is_err());
    }

    #[test]
    fn parse_targets_comma_and_dedupe() {
        let targets = parse_npm_targets(&[
            "linux-x64,darwin-arm64".into(),
            "linux-x64".into(),
        ])
        .unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].label, "linux-x64");
        assert_eq!(targets[1].label, "darwin-arm64");
    }

    #[test]
    fn parse_resolved_tarball_urls() {
        let (n, v) = parse_npm_tarball_resolved(
            "https://registry.npmmirror.com/string-width/-/string-width-4.2.3.tgz",
        )
        .unwrap();
        assert_eq!(n, "string-width");
        assert_eq!(v, "4.2.3");

        let (n, v) = parse_npm_tarball_resolved(
            "https://registry.npmjs.org/@koa/cors/-/cors-5.0.0.tgz",
        )
        .unwrap();
        assert_eq!(n, "@koa/cors");
        assert_eq!(v, "5.0.0");
    }
}
