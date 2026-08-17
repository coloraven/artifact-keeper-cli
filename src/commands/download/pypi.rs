//! PyPI ferry download: isolated `pip download` per root (dep tree or `--no-deps`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use miette::Result;
use serde_json::Value;

use super::cache::{CacheKey, DownloadCache};
use super::config::UpstreamConfig;
use super::engine::{
    self, format_root, trim_err, FetchedModule, RootFetchResult, RootPass, WorkDirs,
};
use super::errors::UnitError;
use super::manifest::{file_entry, parse_module_list, FerryManifest, ModuleEntry, RootSpec};
use crate::error::AkError;
use crate::output::OutputFormat;

pub async fn download_pypi(
    input: &Path,
    work: &Path,
    output: &Path,
    all_versions: bool,
    platforms: &[String],
    pythons: &[String],
    upstream: &UpstreamConfig,
    cache: Arc<DownloadCache>,
    catalog: Option<Arc<super::catalog::ServerCatalog>>,
    jobs: usize,
    format: &OutputFormat,
) -> Result<()> {
    if all_versions {
        if (!platforms.is_empty() || !pythons.is_empty())
            && !matches!(format, OutputFormat::Quiet)
        {
            eprintln!(
                "note: --target/--node are ignored with --all-versions (root dists only, --no-deps)"
            );
        }
    } else if (!platforms.is_empty() || !pythons.is_empty())
        && !matches!(format, OutputFormat::Quiet)
    {
        eprintln!(
            "pypi platforms: {}; python: {}",
            if platforms.is_empty() {
                "host".into()
            } else {
                platforms.join(",")
            },
            if pythons.is_empty() {
                "host".into()
            } else {
                pythons.join(",")
            }
        );
    }

    let tool = Arc::new(PypiToolchain {
        platforms: platforms.to_vec(),
        pythons: pythons.to_vec(),
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
        },
    )
    .await
}

struct PypiToolchain {
    platforms: Vec<String>,
    pythons: Vec<String>,
    all_versions: bool,
}

#[derive(Debug, Clone, Default)]
struct PypiCtx {
    pack_only: bool,
    platform: Option<String>,
    python: Option<String>,
}

fn encode_ctx(ctx: &PypiCtx) -> String {
    serde_json::json!({
        "pack_only": ctx.pack_only,
        "platform": ctx.platform,
        "python": ctx.python,
    })
    .to_string()
}

fn decode_ctx(raw: &str) -> PypiCtx {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return PypiCtx::default();
    };
    PypiCtx {
        pack_only: v
            .get("pack_only")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        platform: v
            .get("platform")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        python: v
            .get("python")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    }
}

impl engine::LanguageToolchain for PypiToolchain {
    fn ecosystem(&self) -> &'static str {
        "pypi"
    }
    fn artifact_subdir(&self) -> &'static str {
        "pypi"
    }
    fn tool_cache_subdir(&self) -> &'static str {
        "pip-cache"
    }
    fn ensure_toolchain(&self) -> Result<()> {
        ensure_pip_toolchain()
    }
    fn resolve_roots(&self, input: &Path) -> Result<Vec<RootSpec>> {
        resolve_pypi_roots(input)
    }
    fn expand_all_versions(
        &self,
        roots: Vec<RootSpec>,
        upstream: &UpstreamConfig,
        format: &OutputFormat,
        work: &Path,
    ) -> Result<Vec<RootSpec>> {
        expand_pypi_all_versions(&roots, upstream, format, work)
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
                    context: encode_ctx(&PypiCtx {
                        pack_only: true,
                        ..Default::default()
                    }),
                })
                .collect();
        }

        let platforms: Vec<Option<String>> = if self.platforms.is_empty() {
            vec![None]
        } else {
            self.platforms.iter().cloned().map(Some).collect()
        };
        let pythons: Vec<Option<String>> = if self.pythons.is_empty() {
            vec![None]
        } else {
            self.pythons.iter().cloned().map(Some).collect()
        };
        let total = roots.len() * platforms.len() * pythons.len();
        let mut out = Vec::with_capacity(total);
        let mut pass_idx = 0usize;
        for root in roots {
            for platform in &platforms {
                for python in &pythons {
                    pass_idx += 1;
                    out.push(RootPass {
                        pass_idx,
                        total,
                        root: root.clone(),
                        context: encode_ctx(&PypiCtx {
                            pack_only: false,
                            platform: platform.clone(),
                            python: python.clone(),
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
        let ctx = decode_ctx(&pass.context);
        let root = &pass.root;
        let spec = format_root(root);
        let Some(ver) = root
            .version
            .as_deref()
            .filter(|v| is_exact_pypi_version(v))
        else {
            return Ok(false);
        };
        if ctx.pack_only {
            if known.contains(&(root.name.clone(), ver.to_string())) {
                return Ok(true);
            }
            let key = CacheKey::pypi(&root.name, ver, None, None);
            if !cache.should_skip(&key)? {
                return Ok(false);
            }
            if !matches!(*format, OutputFormat::Quiet) {
                eprintln!("[{}/{}] skip cached {spec}", pass.pass_idx, pass.total);
            }
            restore_pypi_closure(
                cache,
                &key,
                &dirs.artifacts,
                &dirs.payload,
                known,
                manifest,
                &format!("{spec} (all-versions, cached)"),
            )?;
            return Ok(true);
        }

        let plat = ctx.platform.as_deref();
        let py = ctx.python.as_deref();
        let key = CacheKey::pypi(&root.name, ver, plat, py);
        if !cache.should_skip(&key)? {
            return Ok(false);
        }
        let plat_disp = plat.unwrap_or("host");
        let py_disp = py.unwrap_or("host");
        if !matches!(*format, OutputFormat::Quiet) {
            eprintln!(
                "[{}/{}] skip cached pip download {spec} (platform={plat_disp}, python={py_disp})",
                pass.pass_idx, pass.total
            );
        }
        restore_pypi_closure(
            cache,
            &key,
            &dirs.artifacts,
            &dirs.payload,
            known,
            manifest,
            &format!("{spec} [{plat_disp}/py-{py_disp}] (cached)"),
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
        let ctx = decode_ctx(&pass.context);
        if ctx.pack_only {
            fetch_pack_only(pass, &dirs, &upstream, &cache, quiet)
        } else {
            fetch_with_deps(pass, ctx, &dirs, &upstream, &cache, quiet)
        }
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
    let spec = format!("{}=={version}", root.name);
    if !quiet {
        eprintln!(
            "[{}/{}] pip download --no-deps {spec}",
            pass.pass_idx, pass.total
        );
    }
    let dest = dirs.work.join(format!("pack-{}", pass.pass_idx));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| {
        UnitError::new(format!("pip download {spec}"), format!("mkdir: {e}"))
    })?;

    run_pip_download(
        &dest,
        &dirs.tool_cache,
        &dirs.work,
        &req_spec(&root.name, Some(version)),
        upstream,
        true,
        None,
        None,
    )
    .map_err(|e| UnitError::new(format!("pip download {spec}"), trim_err(&e.to_string())))?;

    let files = collect_dists(&dest).map_err(|e| {
        UnitError::new(format!("pip download {spec}"), trim_err(&e.to_string()))
    })?;
    if files.is_empty() {
        return Err(UnitError::new(
            format!("pip download {spec}"),
            "no wheel/sdist produced",
        ));
    }

    let mut modules = Vec::new();
    let mut soft_errors = Vec::new();
    for src in files {
        match stage_dist(&src, &dirs.artifacts) {
            Ok((name, ver, dest_path)) => {
                let key = CacheKey::pypi(&name, &ver, None, None);
                let _ = cache.store(&key, &[dest_path.clone()]);
                modules.push(FetchedModule {
                    name,
                    version: ver,
                    name_encoded: None,
                    files: vec![dest_path],
                });
            }
            Err(e) => soft_errors.push(UnitError::new(
                format!("stage {}", src.display()),
                trim_err(&e.to_string()),
            )),
        }
    }
    let key = CacheKey::pypi(&root.name, version, None, None);
    let deps: Vec<_> = modules
        .iter()
        .map(|m| (m.name.clone(), m.version.clone()))
        .collect();
    let _ = cache.store_closure(&key, &deps);

    Ok(RootFetchResult {
        via: format!("{} (all-versions)", root.name),
        modules,
        soft_errors,
    })
}

fn fetch_with_deps(
    pass: RootPass,
    ctx: PypiCtx,
    dirs: &WorkDirs,
    upstream: &UpstreamConfig,
    cache: &DownloadCache,
    quiet: bool,
) -> std::result::Result<RootFetchResult, UnitError> {
    let root = &pass.root;
    let spec = format_root(root);
    let plat_disp = ctx.platform.as_deref().unwrap_or("host");
    let py_disp = ctx.python.as_deref().unwrap_or("host");
    if !quiet {
        eprintln!(
            "[{}/{}] pip download {spec} (platform={plat_disp}, python={py_disp})",
            pass.pass_idx, pass.total
        );
    }

    let dest = dirs.work.join(format!("dl-{}", pass.pass_idx));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| {
        UnitError::new(format!("pip download {spec}"), format!("mkdir: {e}"))
    })?;

    let req = req_spec(
        &root.name,
        root.version.as_deref(),
    );
    run_pip_download(
        &dest,
        &dirs.tool_cache,
        &dirs.work,
        &req,
        upstream,
        false,
        ctx.platform.as_deref(),
        ctx.python.as_deref(),
    )
    .map_err(|e| UnitError::new(format!("pip download {spec}"), trim_err(&e.to_string())))?;

    let files = collect_dists(&dest).map_err(|e| {
        UnitError::new(format!("pip download {spec}"), trim_err(&e.to_string()))
    })?;
    if files.is_empty() {
        return Err(UnitError::new(
            format!("pip download {spec}"),
            "no wheel/sdist produced",
        ));
    }

    let plat = ctx.platform.as_deref();
    let py = ctx.python.as_deref();
    let mut modules = Vec::new();
    let mut soft_errors = Vec::new();
    let mut closure_deps = Vec::new();

    for src in files {
        match stage_dist(&src, &dirs.artifacts) {
            Ok((name, ver, dest_path)) => {
                let key = CacheKey::pypi(&name, &ver, plat, py);
                if !cache.force() && cache.contains(&key).unwrap_or(false) {
                    // already cached; still include in ferry
                } else {
                    let _ = cache.store(&key, &[dest_path.clone()]);
                }
                closure_deps.push((name.clone(), ver.clone()));
                modules.push(FetchedModule {
                    name,
                    version: ver,
                    name_encoded: None,
                    files: vec![dest_path],
                });
            }
            Err(e) => soft_errors.push(UnitError::new(
                format!("stage {}", src.display()),
                trim_err(&e.to_string()),
            )),
        }
    }

    let root_ver = modules
        .iter()
        .find(|m| normalize_pypi_name(&m.name) == normalize_pypi_name(&root.name))
        .map(|m| m.version.clone())
        .or_else(|| root.version.clone())
        .unwrap_or_default();
    if !root_ver.is_empty() {
        let root_key = CacheKey::pypi(&root.name, &root_ver, plat, py);
        let _ = cache.store_closure(&root_key, &closure_deps);
    }

    Ok(RootFetchResult {
        via: format!("{spec} [{plat_disp}/py-{py_disp}]"),
        modules,
        soft_errors,
    })
}

fn req_spec(name: &str, version: Option<&str>) -> String {
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if is_exact_pypi_version(v) => format!("{name}=={v}"),
        Some(v) if v.starts_with(['=', '>', '<', '~', '!']) => format!("{name}{v}"),
        Some(v) => format!("{name}=={v}"),
        None => name.to_string(),
    }
}

fn ensure_pip_toolchain() -> Result<()> {
    let (prog, prefix) = pip_invocation()?;
    let mut cmd = Command::new(&prog);
    cmd.args(&prefix).arg("--version");
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err(AkError::ConfigError(
            "`python -m pip` / `pip` not found on PATH; install Python/pip to use `ak download --pypi`"
                .into(),
        )
        .into()),
    }
}

/// `(program, args_before_pip_subcommand)` e.g. `("python", ["-m", "pip"])`.
fn pip_invocation() -> Result<(String, Vec<String>)> {
    let candidates: Vec<(String, Vec<String>)> = if cfg!(windows) {
        vec![
            ("py".into(), vec!["-3".into(), "-m".into(), "pip".into()]),
            ("python".into(), vec!["-m".into(), "pip".into()]),
            ("python3".into(), vec!["-m".into(), "pip".into()]),
            ("pip".into(), vec![]),
            ("pip3".into(), vec![]),
        ]
    } else {
        vec![
            ("python3".into(), vec!["-m".into(), "pip".into()]),
            ("python".into(), vec!["-m".into(), "pip".into()]),
            ("pip3".into(), vec![]),
            ("pip".into(), vec![]),
        ]
    };
    for (prog, prefix) in candidates {
        let mut cmd = Command::new(&prog);
        cmd.args(&prefix).arg("--version");
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if cmd.status().map(|s| s.success()).unwrap_or(false) {
            return Ok((prog, prefix));
        }
    }
    Err(AkError::ConfigError("pip not found".into()).into())
}

fn run_pip_download(
    dest: &Path,
    pip_cache: &Path,
    work: &Path,
    requirement: &str,
    upstream: &UpstreamConfig,
    no_deps: bool,
    platform: Option<&str>,
    python: Option<&str>,
) -> Result<()> {
    let (prog, prefix) = pip_invocation()?;
    let mut cmd = Command::new(&prog);
    cmd.args(&prefix).args([
        "download",
        "-d",
    ]);
    cmd.arg(dest);
    cmd.arg("--cache-dir").arg(pip_cache);
    cmd.arg("--index-url").arg(upstream.pypi_index());
    cmd.arg("--disable-pip-version-check");
    cmd.env("PIP_CACHE_DIR", pip_cache);
    cmd.env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
    engine::apply_isolated_temp(&mut cmd, work);
    if no_deps {
        cmd.arg("--no-deps");
    }
    if let Some(plat) = platform {
        let py = python
            .map(|s| s.to_string())
            .or_else(|| detect_host_python_version().ok())
            .unwrap_or_else(|| "3.12".into());
        let (impl_tag, abi) = python_abi_tags(&py);
        cmd.arg("--only-binary=:all:");
        cmd.arg("--platform").arg(plat);
        cmd.arg("--python-version").arg(normalize_python_version(&py));
        cmd.arg("--implementation").arg(impl_tag);
        cmd.arg("--abi").arg(abi);
    } else if let Some(py) = python {
        let _ = py;
    }
    cmd.arg(requirement);

    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn pip: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "pip download {requirement} failed:\n{stderr}"
        ))
        .into());
    }
    Ok(())
}

fn detect_host_python_version() -> Result<String> {
    let (prog, prefix) = pip_invocation()?;
    // prefix ends with "pip" when using -m pip; use the python prog itself
    let py = if prefix.windows(2).any(|w| w == ["-m", "pip"]) {
        prog
    } else if prog == "py" {
        "py".into()
    } else {
        "python3".into()
    };
    let mut cmd = if py == "py" {
        let mut c = Command::new("py");
        c.args(["-3", "-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"]);
        c
    } else {
        let mut c = Command::new(&py);
        c.args([
            "-c",
            "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')",
        ]);
        c
    };
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("detect python version: {e}")))?;
    if !output.status.success() {
        return Err(AkError::ConfigError("detect python version failed".into()).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn normalize_python_version(v: &str) -> String {
    // pip --python-version wants e.g. 311 or 3.11
    let v = v.trim().trim_start_matches('v');
    if v.contains('.') {
        v.replace('.', "")
    } else {
        v.to_string()
    }
}

fn python_abi_tags(py: &str) -> (&'static str, String) {
    let digits = normalize_python_version(py);
    ("cp", format!("cp{digits}"))
}

fn collect_dists(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for ent in std::fs::read_dir(dir)
        .map_err(|e| AkError::ConfigError(format!("read {}: {e}", dir.display())))?
    {
        let ent = ent.map_err(|e| AkError::ConfigError(format!("read dir: {e}")))?;
        let p = ent.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".whl")
            || name.ends_with(".tar.gz")
            || name.ends_with(".zip")
            || name.ends_with(".tar.bz2")
        {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn stage_dist(src: &Path, artifacts: &Path) -> Result<(String, String, PathBuf)> {
    let fname = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AkError::ConfigError("dist missing filename".into()))?;
    let (name, version) = parse_dist_filename(fname).ok_or_else(|| {
        AkError::ConfigError(format!("cannot parse dist filename: {fname}"))
    })?;
    let dest_dir = artifacts.join(sanitize_name(&name));
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", dest_dir.display())))?;
    let dest = dest_dir.join(fname);
    if src != dest {
        std::fs::copy(src, &dest)
            .map_err(|e| AkError::ConfigError(format!("copy {fname}: {e}")))?;
    }
    Ok((name, version, dest))
}

/// Parse wheel / sdist filename → (name, version).
pub fn parse_dist_filename(fname: &str) -> Option<(String, String)> {
    if let Some(stem) = fname.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        // {name}-{ver}(-{build})?-{py}-{abi}-{plat}
        if parts.len() >= 5 {
            let name = parts[0].replace('_', "-");
            let version = parts[1].to_string();
            return Some((name, version));
        }
        return None;
    }
    for suffix in [".tar.gz", ".tar.bz2", ".zip"] {
        if let Some(stem) = fname.strip_suffix(suffix) {
            // Prefer last `-` segment that looks like a version.
            if let Some((name, ver)) = split_name_version(stem) {
                return Some((name.replace('_', "-"), ver));
            }
        }
    }
    None
}

fn split_name_version(stem: &str) -> Option<(String, String)> {
    let mut idx = stem.len();
    while let Some(dash) = stem[..idx].rfind('-') {
        let ver = &stem[dash + 1..];
        if ver
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            return Some((stem[..dash].to_string(), ver.to_string()));
        }
        idx = dash;
        if dash == 0 {
            break;
        }
    }
    None
}

fn sanitize_name(name: &str) -> String {
    normalize_pypi_name(name)
}

fn normalize_pypi_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '_' || c == '.' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

fn is_exact_pypi_version(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty()
        && !v.contains(['>', '<', '!', '|', ' ', '*'])
        && !v.starts_with('~')
        && !v.eq_ignore_ascii_case("latest")
}

fn restore_pypi_closure(
    cache: &DownloadCache,
    root_key: &CacheKey,
    artifacts: &Path,
    payload: &Path,
    known: &mut HashSet<(String, String)>,
    manifest: &mut FerryManifest,
    via: &str,
) -> Result<()> {
    for (name, version) in cache.closure_members(root_key)? {
        if !known.insert((name.clone(), version.clone())) {
            continue;
        }
        let key = CacheKey::pypi(
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
        let dest = artifacts.join(sanitize_name(&name));
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

fn expand_pypi_all_versions(
    roots: &[RootSpec],
    upstream: &UpstreamConfig,
    format: &OutputFormat,
    work: &Path,
) -> Result<Vec<RootSpec>> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let n = normalize_pypi_name(&root.name);
        if seen.insert(n.clone()) {
            names.push(root.name.clone());
        }
    }
    let pip_cache = work.join("pip-cache");
    std::fs::create_dir_all(&pip_cache)
        .map_err(|e| AkError::ConfigError(format!("mkdir pip-cache: {e}")))?;
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if !matches!(format, OutputFormat::Quiet) {
            eprintln!(
                "  [{}/{}] pip index versions {name}",
                i + 1,
                names.len()
            );
        }
        let versions = pypi_list_versions(name, upstream, work, &pip_cache)?;
        if versions.is_empty() {
            return Err(AkError::ConfigError(format!(
                "pip index versions {name} returned no versions"
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

fn pypi_list_versions(
    name: &str,
    upstream: &UpstreamConfig,
    work: &Path,
    pip_cache: &Path,
) -> Result<Vec<String>> {
    let (prog, prefix) = pip_invocation()?;
    let mut cmd = Command::new(&prog);
    cmd.args(&prefix)
        .args(["index", "versions", name])
        .arg("--cache-dir")
        .arg(pip_cache)
        .arg("--index-url")
        .arg(upstream.pypi_index())
        .arg("--disable-pip-version-check")
        .env("PIP_CACHE_DIR", pip_cache)
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
    engine::apply_isolated_temp(&mut cmd, work);
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn pip index: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "pip index versions {name} failed:\n{stderr}"
        ))
        .into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_pip_index_versions(&stdout)
}

fn parse_pip_index_versions(stdout: &str) -> Result<Vec<String>> {
    // "Available versions: 2.31.0, 2.30.0, ..."
    for line in stdout.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("Available versions:")
            .or_else(|| line.strip_prefix("available versions:"))
        else {
            continue;
        };
        let vers: Vec<String> = rest
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !vers.is_empty() {
            return Ok(vers);
        }
    }
    Err(AkError::ConfigError(format!(
        "Could not parse pip index versions output:\n{stdout}"
    ))
    .into())
}

fn resolve_pypi_roots(input: &Path) -> Result<Vec<RootSpec>> {
    let text = std::fs::read_to_string(input)
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", input.display())))?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "requirements.txt"
        || name.ends_with(".txt")
            && text.lines().any(|l| {
                let t = l.trim();
                !t.is_empty()
                    && !t.starts_with('#')
                    && (t.contains("==") || t.contains(">=") || t.contains("~="))
            })
    {
        Ok(parse_requirements_txt(&text))
    } else {
        Ok(parse_module_list(&text))
    }
}

fn parse_requirements_txt(text: &str) -> Vec<RootSpec> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        // strip env markers: pkg==1.0 ; python_version>="3"
        let line = line.split(';').next().unwrap_or(line).trim();
        if let Some(spec) = parse_requirement_line(line) {
            out.push(spec);
        }
    }
    out
}

fn parse_requirement_line(line: &str) -> Option<RootSpec> {
    // name==1.2.3 / name>=1 / name[extra]==1.0
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (name_part, version) = if let Some(i) = line.find("==") {
        (&line[..i], Some(line[i + 2..].trim().to_string()))
    } else if let Some(i) = line.find(">=") {
        (&line[..i], Some(format!(">={}", line[i + 2..].trim())))
    } else if let Some(i) = line.find("<=") {
        (&line[..i], Some(format!("<={}", line[i + 2..].trim())))
    } else if let Some(i) = line.find('~') {
        // ~=
        if line[i..].starts_with("~=") {
            (&line[..i], Some(format!("~={}", line[i + 2..].trim())))
        } else {
            (line, None)
        }
    } else if let Some(i) = line.find('>') {
        (&line[..i], Some(format!(">{}", line[i + 1..].trim())))
    } else if let Some(i) = line.find('<') {
        (&line[..i], Some(format!("<{}", line[i + 1..].trim())))
    } else {
        (line, None)
    };
    let name = name_part
        .split('[')
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    Some(RootSpec { name, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wheel_and_sdist_names() {
        let (n, v) = parse_dist_filename("requests-2.31.0-py3-none-any.whl").unwrap();
        assert_eq!(n, "requests");
        assert_eq!(v, "2.31.0");
        let (n, v) = parse_dist_filename("charset_normalizer-3.3.2-py3-none-any.whl").unwrap();
        assert_eq!(n, "charset-normalizer");
        assert_eq!(v, "3.3.2");
        let (n, v) = parse_dist_filename("requests-2.31.0.tar.gz").unwrap();
        assert_eq!(n, "requests");
        assert_eq!(v, "2.31.0");
    }

    #[test]
    fn parse_requirements() {
        let text = r#"
# comment
requests==2.31.0
urllib3>=2.0
flask[async]==3.0.0 ; python_version>="3.8"
"#;
        let roots = parse_requirements_txt(text);
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].name, "requests");
        assert_eq!(roots[0].version.as_deref(), Some("2.31.0"));
        assert_eq!(roots[1].version.as_deref(), Some(">=2.0"));
        assert_eq!(roots[2].name, "flask");
        assert_eq!(roots[2].version.as_deref(), Some("3.0.0"));
    }

    #[test]
    fn parse_pip_index_output() {
        let out = "requests (2.31.0)\nAvailable versions: 2.31.0, 2.30.0, 2.29.0\n";
        let v = parse_pip_index_versions(out).unwrap();
        assert_eq!(v, vec!["2.31.0", "2.30.0", "2.29.0"]);
    }
}
