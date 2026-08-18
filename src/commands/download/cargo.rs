//! Cargo / crates.io ferry download: isolated `cargo fetch` or direct `.crate` fetch.

use std::collections::{HashMap, HashSet};
use std::io::Write;
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

pub async fn download_cargo(
    input: &Path,
    work: &Path,
    output: &Path,
    all_versions: bool,
    upstream: &UpstreamConfig,
    cache: Arc<DownloadCache>,
    catalog: Option<Arc<super::catalog::ServerCatalog>>,
    jobs: usize,
    format: &OutputFormat,
    auto_name: bool,
    no_archive: bool,
    verbose: bool,
) -> Result<()> {
    let tool = Arc::new(CargoToolchain { all_versions });
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
            no_archive,
            verbose,
        },
    )
    .await
}

struct CargoToolchain {
    all_versions: bool,
}

#[derive(Debug, Clone, Default)]
struct CargoCtx {
    /// all-versions: download root `.crate` only (no dep tree).
    pack_only: bool,
}

fn encode_ctx(ctx: &CargoCtx) -> String {
    serde_json::json!({ "pack_only": ctx.pack_only }).to_string()
}

fn decode_ctx(raw: &str) -> CargoCtx {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return CargoCtx::default();
    };
    CargoCtx {
        pack_only: v
            .get("pack_only")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    }
}

impl engine::LanguageToolchain for CargoToolchain {
    fn ecosystem(&self) -> &'static str {
        "cargo"
    }
    fn artifact_subdir(&self) -> &'static str {
        "cargo"
    }
    fn tool_cache_subdir(&self) -> &'static str {
        "cargo-home"
    }
    fn ensure_toolchain(&self) -> Result<()> {
        ensure_cargo_toolchain()
    }
    fn resolve_roots(&self, input: &Path) -> Result<Vec<RootSpec>> {
        resolve_cargo_roots(input)
    }
    fn expand_all_versions(
        &self,
        roots: Vec<RootSpec>,
        upstream: &UpstreamConfig,
        format: &OutputFormat,
        work: &Path,
    ) -> Result<Vec<RootSpec>> {
        expand_cargo_all_versions(&roots, upstream, format, work)
    }
    fn expand_passes(&self, roots: Vec<RootSpec>) -> Vec<RootPass> {
        let total = roots.len();
        roots
            .into_iter()
            .enumerate()
            .map(|(i, root)| RootPass {
                pass_idx: i + 1,
                total,
                root,
                context: encode_ctx(&CargoCtx {
                    pack_only: self.all_versions,
                }),
            })
            .collect()
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
        let root = &pass.root;
        let Some(ver) = root
            .version
            .as_deref()
            .filter(|v| is_exact_cargo_version(v))
        else {
            return Ok(false);
        };
        let spec = format_root(root);
        let key = CacheKey::cargo(&root.name, ver);
        if !cache.should_skip(&key)? {
            return Ok(false);
        }
        let ctx = decode_ctx(&pass.context);
        if !matches!(*format, OutputFormat::Quiet) {
            eprintln!(
                "[{}/{}] skip cached cargo {} {spec}",
                pass.pass_idx,
                pass.total,
                if ctx.pack_only { "crate" } else { "fetch" }
            );
        }
        restore_cargo_closure(
            cache,
            &key,
            &dirs.artifacts,
            &dirs.payload,
            known,
            manifest,
            &format!(
                "{spec} ({})",
                if ctx.pack_only {
                    "all-versions, cached"
                } else {
                    "cached"
                }
            ),
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
            return fetch_crate_only(pass, &dirs, &upstream, &cache, quiet);
        }
        // Serialize CARGO_HOME mutations (shared registry cache).
        static CARGO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CARGO_LOCK
            .lock()
            .map_err(|e| UnitError::new("cargo lock", e.to_string()))?;
        fetch_with_deps(pass, &dirs, &upstream, &cache, quiet)
    }
}

fn fetch_crate_only(
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
        eprintln!(
            "[{}/{}] download crate {spec}",
            pass.pass_idx, pass.total
        );
    }
    let dest = download_crate_file(&root.name, version, &dirs.artifacts, upstream)
        .map_err(|e| UnitError::new(format!("crate {spec}"), trim_err(&e.to_string())))?;
    let key = CacheKey::cargo(&root.name, version);
    cache
        .store(&key, &[dest.clone()])
        .map_err(|e| UnitError::new(format!("cache store {spec}"), e.to_string()))?;
    let _ = cache.store_closure(&key, &[(root.name.clone(), version.to_string())]);
    Ok(RootFetchResult {
        via: format!("{} (all-versions)", root.name),
        modules: vec![FetchedModule {
            name: root.name.clone(),
            version: version.to_string(),
            name_encoded: None,
            files: vec![dest],
        }],
        soft_errors: Vec::new(),
    })
}

fn fetch_with_deps(
    pass: RootPass,
    dirs: &WorkDirs,
    upstream: &UpstreamConfig,
    cache: &DownloadCache,
    quiet: bool,
) -> std::result::Result<RootFetchResult, UnitError> {
    let root = &pass.root;
    let spec = format_root(root);
    if !quiet {
        eprintln!(
            "[{}/{}] cargo fetch {spec} (isolated)",
            pass.pass_idx, pass.total
        );
    }

    write_cargo_home_config(&dirs.tool_cache, upstream).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), trim_err(&e.to_string()))
    })?;

    let isolate = dirs.work.join(format!("isolate-{}", pass.pass_idx));
    let _ = std::fs::remove_dir_all(&isolate);
    std::fs::create_dir_all(&isolate).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), format!("mkdir isolate: {e}"))
    })?;

    let dep_line = match &root.version {
        Some(v) if is_exact_cargo_version(v) => format!("{} = \"={v}\"", root.name),
        Some(v) if !v.is_empty() => format!("{} = \"{v}\"", root.name),
        _ => format!("{} = \"*\"", root.name),
    };
    // Explicit `[workspace]` makes this isolate its own workspace root so
    // `cargo fetch` does not walk up into a parent Cargo.toml (e.g. when the
    // work-dir lives inside another Rust git checkout).
    let cargo_toml = format!(
        "[workspace]\n\
         \n\
         [package]\n\
         name = \"ak-ferry-isolate\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         \n\
         [dependencies]\n\
         {dep_line}\n"
    );
    std::fs::write(isolate.join("Cargo.toml"), cargo_toml).map_err(|e| {
        UnitError::new(
            format!("cargo fetch {spec}"),
            format!("write Cargo.toml: {e}"),
        )
    })?;
    // Empty lib so cargo accepts the package.
    std::fs::create_dir_all(isolate.join("src")).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), format!("mkdir src: {e}"))
    })?;
    std::fs::write(isolate.join("src").join("lib.rs"), "// ak-ferry\n").map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), format!("write lib.rs: {e}"))
    })?;

    let before = snapshot_crate_files(&dirs.tool_cache).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), e.to_string())
    })?;
    run_cargo_fetch(&isolate, &dirs.tool_cache, &dirs.work).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), trim_err(&e.to_string()))
    })?;
    let after = snapshot_crate_files(&dirs.tool_cache).map_err(|e| {
        UnitError::new(format!("cargo fetch {spec}"), e.to_string())
    })?;

    let lock_pkgs = parse_cargo_lock_packages(&isolate.join("Cargo.lock")).unwrap_or_default();
    let mut modules = Vec::new();
    let mut soft_errors = Vec::new();
    let mut closure_deps = Vec::new();

    for path in after.difference(&before) {
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let Some((name, version)) = parse_crate_filename(fname)
            .or_else(|| infer_from_lock(fname, &lock_pkgs))
        else {
            soft_errors.push(UnitError::new(
                fname.to_string(),
                "cannot parse .crate filename",
            ));
            continue;
        };
        match stage_crate(path, &dirs.artifacts, &name, &version) {
            Ok(dest) => {
                let key = CacheKey::cargo(&name, &version);
                let _ = cache.store(&key, &[dest.clone()]);
                closure_deps.push((name.clone(), version.clone()));
                modules.push(FetchedModule {
                    name,
                    version,
                    name_encoded: None,
                    files: vec![dest],
                });
            }
            Err(e) => soft_errors.push(UnitError::new(
                format!("{name}@{version}"),
                trim_err(&e.to_string()),
            )),
        }
    }

    // Ensure root is in closure even if filename parse failed to match.
    let root_ver = modules
        .iter()
        .find(|m| m.name == root.name)
        .map(|m| m.version.clone())
        .or_else(|| root.version.clone())
        .or_else(|| {
            lock_pkgs
                .get(&root.name)
                .and_then(|vs| vs.first().cloned())
        })
        .unwrap_or_default();
    if !root_ver.is_empty() {
        let root_key = CacheKey::cargo(&root.name, &root_ver);
        let _ = cache.store_closure(&root_key, &closure_deps);
    }

    if modules.is_empty() {
        return Err(UnitError::new(
            format!("cargo fetch {spec}"),
            "no .crate files produced",
        ));
    }

    Ok(RootFetchResult {
        via: spec,
        modules,
        soft_errors,
    })
}

fn ensure_cargo_toolchain() -> Result<()> {
    let status = Command::new("cargo")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(AkError::ConfigError(
            "`cargo` not found on PATH; install Rust/cargo to use `ak download --cargo`".into(),
        )
        .into()),
    }
}

fn write_cargo_home_config(cargo_home: &Path, upstream: &UpstreamConfig) -> Result<()> {
    std::fs::create_dir_all(cargo_home)
        .map_err(|e| AkError::ConfigError(format!("mkdir cargo-home: {e}")))?;
    let config = format!(
        "[source.crates-io]\n\
         replace-with = \"ak-ferry\"\n\
         \n\
         [source.ak-ferry]\n\
         registry = \"{}\"\n",
        upstream.cargo_sparse_registry()
    );
    std::fs::write(cargo_home.join("config.toml"), config)
        .map_err(|e| AkError::ConfigError(format!("write cargo config: {e}")))?;
    Ok(())
}

fn run_cargo_fetch(isolate: &Path, cargo_home: &Path, work: &Path) -> Result<()> {
    // Resolve + download into CARGO_HOME (creates Cargo.lock as a side effect).
    let target_dir = isolate.join("target");
    let mut cmd = Command::new("cargo");
    cmd.current_dir(isolate)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", &target_dir)
        .arg("fetch");
    super::engine::apply_isolated_temp(&mut cmd, work);
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn cargo: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!("cargo fetch failed:\n{stderr}")).into());
    }
    if !isolate.join("Cargo.lock").is_file() {
        let mut cmd = Command::new("cargo");
        cmd.current_dir(isolate)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args(["generate-lockfile"]);
        super::engine::apply_isolated_temp(&mut cmd, work);
        let _ = cmd.status();
    }
    Ok(())
}

fn snapshot_crate_files(cargo_home: &Path) -> Result<HashSet<PathBuf>> {
    let mut out = HashSet::new();
    let cache_root = cargo_home.join("registry").join("cache");
    if !cache_root.is_dir() {
        return Ok(out);
    }
    visit_crates(&cache_root, &mut out)?;
    Ok(out)
}

fn visit_crates(dir: &Path, out: &mut HashSet<PathBuf>) -> Result<()> {
    for ent in std::fs::read_dir(dir)
        .map_err(|e| AkError::ConfigError(format!("read {}: {e}", dir.display())))?
    {
        let ent = ent.map_err(|e| AkError::ConfigError(format!("read dir: {e}")))?;
        let p = ent.path();
        if p.is_dir() {
            visit_crates(&p, out)?;
        } else if p.extension().and_then(|e| e.to_str()) == Some("crate") {
            out.insert(p);
        }
    }
    Ok(())
}

fn stage_crate(src: &Path, artifacts: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let dest_dir = artifacts.join(name);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", dest_dir.display())))?;
    let dest = dest_dir.join(format!("{name}-{version}.crate"));
    if src != dest {
        std::fs::copy(src, &dest).map_err(|e| {
            AkError::ConfigError(format!("copy {}: {e}", src.display()))
        })?;
    }
    Ok(dest)
}

/// `{name}-{version}.crate` — version starts with a digit.
pub fn parse_crate_filename(fname: &str) -> Option<(String, String)> {
    let stem = fname.strip_suffix(".crate")?;
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

fn infer_from_lock(fname: &str, lock: &HashMap<String, Vec<String>>) -> Option<(String, String)> {
    let stem = fname.strip_suffix(".crate")?;
    for (name, versions) in lock {
        for ver in versions {
            if stem == format!("{name}-{ver}") {
                return Some((name.clone(), ver.clone()));
            }
        }
    }
    None
}

fn parse_cargo_lock_packages(path: &Path) -> Result<HashMap<String, Vec<String>>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| AkError::ConfigError(format!("read Cargo.lock: {e}")))?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            if let (Some(n), Some(v)) = (name.take(), version.take()) {
                map.entry(n).or_default().push(v);
            }
            in_package = true;
            continue;
        }
        if !in_package {
            continue;
        }
        if line.starts_with('[') {
            if let (Some(n), Some(v)) = (name.take(), version.take()) {
                map.entry(n).or_default().push(v);
            }
            in_package = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("name = ") {
            name = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = line.strip_prefix("version = ") {
            version = Some(rest.trim().trim_matches('"').to_string());
        }
    }
    if let (Some(n), Some(v)) = (name, version) {
        map.entry(n).or_default().push(v);
    }
    Ok(map)
}

fn download_crate_file(
    name: &str,
    version: &str,
    artifacts: &Path,
    upstream: &UpstreamConfig,
) -> Result<PathBuf> {
    let base = upstream.cargo_crate_dl_base().trim_end_matches('/').to_string();
    let url = format!("{base}/{name}/{name}-{version}.crate");
    let dest_dir = artifacts.join(name);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", dest_dir.display())))?;
    let dest = dest_dir.join(format!("{name}-{version}.crate"));

    let client = reqwest::blocking::Client::builder()
        .user_agent("artifact-keeper-cli/ak-download")
        .build()
        .map_err(|e| AkError::ConfigError(format!("http client: {e}")))?;
    let mut resp = client
        .get(&url)
        .send()
        .map_err(|e| AkError::ConfigError(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        // Fallback to crates.io static CDN.
        let fallback = format!(
            "https://static.crates.io/crates/{name}/{name}-{version}.crate"
        );
        resp = client
            .get(&fallback)
            .send()
            .map_err(|e| AkError::ConfigError(format!("GET {fallback}: {e}")))?;
        if !resp.status().is_success() {
            return Err(AkError::ConfigError(format!(
                "download {name}@{version} failed: HTTP {} ({url})",
                resp.status()
            ))
            .into());
        }
    }
    let bytes = resp
        .bytes()
        .map_err(|e| AkError::ConfigError(format!("read body: {e}")))?;
    let mut f = std::fs::File::create(&dest)
        .map_err(|e| AkError::ConfigError(format!("write {}: {e}", dest.display())))?;
    f.write_all(&bytes)
        .map_err(|e| AkError::ConfigError(format!("write {}: {e}", dest.display())))?;
    Ok(dest)
}

fn restore_cargo_closure(
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
        let key = CacheKey::cargo(&name, &version);
        let dest = artifacts.join(&name);
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

fn expand_cargo_all_versions(
    roots: &[RootSpec],
    upstream: &UpstreamConfig,
    format: &OutputFormat,
    _work: &Path,
) -> Result<Vec<RootSpec>> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if seen.insert(root.name.clone()) {
            names.push(root.name.clone());
        }
    }
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if !matches!(format, OutputFormat::Quiet) {
            eprintln!(
                "  [{}/{}] list crate versions {name}",
                i + 1,
                names.len()
            );
        }
        let versions = cargo_list_versions(name, upstream)?;
        if versions.is_empty() {
            return Err(AkError::ConfigError(format!(
                "no published versions for crate {name}"
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

fn cargo_list_versions(name: &str, upstream: &UpstreamConfig) -> Result<Vec<String>> {
    let index = upstream.cargo_sparse_registry();
    let index_url = index
        .strip_prefix("sparse+")
        .unwrap_or(index.as_str())
        .trim_end_matches('/');
    let path = sparse_index_path(name);
    let url = format!("{index_url}/{path}");

    let client = reqwest::blocking::Client::builder()
        .user_agent("artifact-keeper-cli/ak-download")
        .build()
        .map_err(|e| AkError::ConfigError(format!("http client: {e}")))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| AkError::ConfigError(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(AkError::ConfigError(format!(
            "sparse index {name} failed: HTTP {} ({url})",
            resp.status()
        ))
        .into());
    }
    let text = resp
        .text()
        .map_err(|e| AkError::ConfigError(format!("read index: {e}")))?;
    let mut versions = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            AkError::ConfigError(format!("invalid sparse index line for {name}: {e}"))
        })?;
        if v.get("yanked").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(ver) = v.get("vers").and_then(|x| x.as_str()) {
            versions.push(ver.to_string());
        }
    }
    Ok(versions)
}

/// crates.io sparse index path encoding.
pub fn sparse_index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        0 => name,
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

fn is_exact_cargo_version(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty()
        && !v.contains(['*', '^', '~', '>', '<', '=', ',', ' '])
        && v
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
}

fn resolve_cargo_roots(input: &Path) -> Result<Vec<RootSpec>> {
    let text = std::fs::read_to_string(input)
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", input.display())))?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "cargo.toml" || text.trim_start().starts_with('[') {
        parse_cargo_toml_deps(&text)
    } else {
        Ok(parse_module_list(&text))
    }
}

fn parse_cargo_toml_deps(text: &str) -> Result<Vec<RootSpec>> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|e| AkError::ConfigError(format!("Invalid Cargo.toml: {e}")))?;
    let mut out = Vec::new();
    if let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) {
        for (name, spec) in deps {
            let version = match spec {
                toml::Value::String(s) => Some(s.clone()),
                toml::Value::Table(t) => t
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            };
            // Skip path/git-only deps without a version (cannot ferry from crates.io).
            if version.is_none() {
                if let toml::Value::Table(t) = spec {
                    if t.contains_key("path") || t.contains_key("git") {
                        continue;
                    }
                }
            }
            out.push(RootSpec {
                name: name.clone(),
                version,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_paths() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("ab"), "2/ab");
        assert_eq!(sparse_index_path("abc"), "3/a/abc");
        assert_eq!(sparse_index_path("serde"), "se/rd/serde");
        assert_eq!(sparse_index_path("tokio"), "to/ki/tokio");
    }

    #[test]
    fn parse_crate_names() {
        let (n, v) = parse_crate_filename("serde-1.0.210.crate").unwrap();
        assert_eq!(n, "serde");
        assert_eq!(v, "1.0.210");
        let (n, v) = parse_crate_filename("serde_json-1.0.0-alpha.1.crate").unwrap();
        assert_eq!(n, "serde_json");
        assert_eq!(v, "1.0.0-alpha.1");
    }

    #[test]
    fn parse_cargo_toml() {
        let text = r#"
[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1.40", features = ["full"] }
local = { path = "../local" }
"#;
        let roots = parse_cargo_toml_deps(text).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|r| r.name == "serde"));
        assert!(roots.iter().any(|r| r.name == "tokio"));
    }
}
