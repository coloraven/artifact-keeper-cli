//! Go ferry download: isolated `go get` per root into a shared GOMODCACHE.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use miette::Result;

use super::cache::{CacheKey, DownloadCache};
use super::config::UpstreamConfig;
use super::manifest::{file_entry, parse_module_list, FerryManifest, ModuleEntry, RootSpec};
use crate::error::AkError;
use crate::output::OutputFormat;

pub async fn download_go(
    input: &Path,
    work: &Path,
    output: &Path,
    all_versions: bool,
    upstream: &UpstreamConfig,
    cache: std::sync::Arc<DownloadCache>,
    catalog: Option<std::sync::Arc<super::catalog::ServerCatalog>>,
    jobs: usize,
    format: &OutputFormat,
    auto_name: bool,
) -> Result<()> {
    let tool = std::sync::Arc::new(GoToolchain);
    super::engine::run_ferry(
        tool,
        input,
        work,
        output,
        super::engine::FerryOpts {
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

struct GoToolchain;

impl super::engine::LanguageToolchain for GoToolchain {
    fn ecosystem(&self) -> &'static str {
        "go"
    }
    fn push_repo_hint(&self) -> &'static str {
        "go"
    }
    fn artifact_subdir(&self) -> &'static str {
        "download"
    }
    fn tool_cache_subdir(&self) -> &'static str {
        "gomodcache"
    }
    fn ensure_toolchain(&self) -> Result<()> {
        ensure_go_toolchain()
    }
    fn resolve_roots(&self, input: &Path) -> Result<Vec<RootSpec>> {
        resolve_go_roots(input)
    }
    fn expand_all_versions(
        &self,
        roots: Vec<RootSpec>,
        upstream: &UpstreamConfig,
        format: &OutputFormat,
        work: &Path,
    ) -> Result<Vec<RootSpec>> {
        expand_go_all_versions(&roots, upstream, format, work)
    }
    fn try_restore_cached(
        &self,
        pass: &super::engine::RootPass,
        dirs: &super::engine::WorkDirs,
        cache: &DownloadCache,
        known: &mut std::collections::HashSet<(String, String)>,
        manifest: &mut FerryManifest,
        format: &OutputFormat,
    ) -> Result<bool> {
        let root = &pass.root;
        let Some(ver) = root.version.as_deref().filter(|v| !v.is_empty() && *v != "latest") else {
            return Ok(false);
        };
        let root_key = CacheKey::go(&root.name, ver);
        if !cache.should_skip(&root_key)? {
            return Ok(false);
        }
        let spec = super::engine::format_root(root);
        if !matches!(*format, OutputFormat::Quiet) {
            eprintln!(
                "[{}/{}] skip cached go get {spec}",
                pass.pass_idx, pass.total
            );
        }
        restore_go_closure(
            cache,
            &root_key,
            &dirs.artifacts,
            &dirs.payload,
            known,
            manifest,
            &format!("{spec} (cached)"),
        )?;
        Ok(true)
    }
    fn fetch_one(
        &self,
        pass: super::engine::RootPass,
        dirs: super::engine::WorkDirs,
        upstream: UpstreamConfig,
        cache: std::sync::Arc<DownloadCache>,
        quiet: bool,
    ) -> std::result::Result<super::engine::RootFetchResult, super::errors::UnitError> {
        // Serialize GOMODCACHE mutations (shared module cache).
        static GO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = GO_LOCK
            .lock()
            .map_err(|e| super::errors::UnitError::new("go lock", e.to_string()))?;

        let root = &pass.root;
        let spec = super::engine::format_root(root);
        if !quiet {
            eprintln!(
                "[{}/{}] go get {spec} (isolated)",
                pass.pass_idx, pass.total
            );
        }

        let download_root = dirs.tool_cache.join("cache").join("download");
        std::fs::create_dir_all(&download_root).map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), format!("mkdir: {e}"))
        })?;
        std::fs::create_dir_all(&dirs.artifacts).map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), format!("mkdir artifacts: {e}"))
        })?;

        let isolate = dirs.work.join(format!("isolate-{}", pass.pass_idx));
        let _ = std::fs::remove_dir_all(&isolate);
        std::fs::create_dir_all(&isolate).map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), format!("mkdir isolate: {e}"))
        })?;
        std::fs::write(isolate.join("go.mod"), "module ak.ferry.isolate\n\ngo 1.22\n").map_err(
            |e| {
                super::errors::UnitError::new(
                    format!("go get {spec}"),
                    format!("write go.mod: {e}"),
                )
            },
        )?;

        let before = snapshot_versions(&download_root).map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), e.to_string())
        })?;
        run_go_get(
            &isolate,
            &dirs.tool_cache,
            &dirs.gocache(),
            root,
            &upstream,
        )
        .map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), e.to_string())
        })?;
        let after = snapshot_versions(&download_root).map_err(|e| {
            super::errors::UnitError::new(format!("go get {spec}"), e.to_string())
        })?;
        sync_download_tree(&download_root, &dirs.artifacts).map_err(|e| {
            super::errors::UnitError::new(format!("sync after {spec}"), e.to_string())
        })?;

        let mut modules = Vec::new();
        let mut closure_deps = Vec::new();
        let mut soft_errors = Vec::new();

        for (module_encoded, version) in after.difference(&before) {
            let name = crate::commands::go_proxy::decode_go_path(module_encoded);
            closure_deps.push((name.clone(), version.clone()));
            let mod_dir = dirs
                .artifacts
                .join(module_encoded.replace('/', std::path::MAIN_SEPARATOR_STR))
                .join("@v");
            let abs_files = go_version_files(&mod_dir, version);
            let key = CacheKey::go(&name, version);
            if !cache.force() && cache.contains(&key).unwrap_or(false) {
                if abs_files.is_empty() {
                    let _ = cache.materialize(&key, &mod_dir);
                }
            } else if !abs_files.is_empty() {
                let _ = cache.store(&key, &abs_files);
            }
            let files = go_version_files(&mod_dir, version);
            if files.is_empty() {
                soft_errors.push(super::errors::UnitError::new(
                    format!("{name}@{version}"),
                    "no module files after go get",
                ));
                continue;
            }
            modules.push(super::engine::FetchedModule {
                name,
                version: version.clone(),
                name_encoded: Some(module_encoded.clone()),
                files,
            });
        }

        let root_encoded = encode_go_path(&root.name);
        let root_ver = root
            .version
            .clone()
            .unwrap_or_else(|| resolve_selected_version(&download_root, &root_encoded));
        if !root_ver.is_empty() {
            let mod_dir = dirs
                .artifacts
                .join(root_encoded.replace('/', std::path::MAIN_SEPARATOR_STR))
                .join("@v");
            let abs_files = go_version_files(&mod_dir, &root_ver);
            let root_key = CacheKey::go(&root.name, &root_ver);
            if !abs_files.is_empty() {
                let _ = cache.store(&root_key, &abs_files);
            }
            closure_deps.push((root.name.clone(), root_ver.clone()));
            let _ = cache.store_closure(&root_key, &closure_deps);
            if !abs_files.is_empty() {
                modules.push(super::engine::FetchedModule {
                    name: root.name.clone(),
                    version: root_ver,
                    name_encoded: Some(root_encoded),
                    files: abs_files,
                });
            }
        }

        Ok(super::engine::RootFetchResult {
            via: spec,
            modules,
            soft_errors,
        })
    }

    fn after_all_fetches(
        &self,
        dirs: &super::engine::WorkDirs,
        manifest: &mut FerryManifest,
        _format: &OutputFormat,
    ) -> Result<()> {
        let download_root = dirs.tool_cache.join("cache").join("download");
        let _ = sync_download_tree(&download_root, &dirs.artifacts);
        let _ = backfill_manifest_from_tree(&dirs.payload, &dirs.artifacts, manifest);
        Ok(())
    }
}


fn go_version_files(mod_dir: &Path, version: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for suffix in ["zip", "mod", "info"] {
        let abs = mod_dir.join(format!("{version}.{suffix}"));
        if abs.is_file() {
            out.push(abs);
        }
    }
    out
}

fn restore_go_closure(
    cache: &DownloadCache,
    root_key: &CacheKey,
    ferry_download: &Path,
    payload: &Path,
    known: &mut HashSet<(String, String)>,
    manifest: &mut FerryManifest,
    via: &str,
) -> Result<()> {
    for (name, version) in cache.closure_members(root_key)? {
        if !known.insert((name.clone(), version.clone())) {
            continue;
        }
        let key = CacheKey::go(&name, &version);
        let encoded = encode_go_path(&name);
        let mod_dir = ferry_download
            .join(encoded.replace('/', std::path::MAIN_SEPARATOR_STR))
            .join("@v");
        let restored = cache.materialize(&key, &mod_dir)?;
        let files = restored
            .iter()
            .map(|p| file_entry(payload, p))
            .collect::<Result<Vec<_>>>()?;
        manifest.record_module(
            payload,
            ModuleEntry {
                name,
                name_encoded: Some(encoded),
                version,
                files,
                via: via.to_string(),
            },
        )?;
    }
    Ok(())
}

fn ensure_go_toolchain() -> Result<()> {
    let status = Command::new("go")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(AkError::ConfigError(
            "`go` not found on PATH; install the Go toolchain to use `ak download --go`".into(),
        )
        .into()),
    }
}

fn run_go_get(
    isolate: &Path,
    gomodcache: &Path,
    gocache: &Path,
    root: &RootSpec,
    upstream: &UpstreamConfig,
) -> Result<()> {
    let _ = std::fs::create_dir_all(gocache);
    let mut cmd = Command::new("go");
    cmd.current_dir(isolate)
        .env("GOMODCACHE", gomodcache)
        .env("GOCACHE", gocache)
        .env("GOFLAGS", "-modcacherw")
        .env("GOPROXY", upstream.goproxy())
        .env("GOSUMDB", upstream.gosumdb())
        .arg("get");
    super::engine::apply_isolated_temp(&mut cmd, isolate.parent().unwrap_or(isolate));

    let arg = match &root.version {
        Some(v) if !v.is_empty() => format!("{}@{}", root.name, v),
        _ => format!("{}@latest", root.name),
    };
    cmd.arg(&arg);

    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn go: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "go get {arg} failed:\n{stderr}"
        ))
        .into());
    }
    Ok(())
}


fn expand_go_all_versions(
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

    let gomodcache = work.join("gomodcache");
    let gocache = work.join("gocache");
    std::fs::create_dir_all(&gomodcache)
        .map_err(|e| AkError::ConfigError(format!("mkdir gomodcache: {e}")))?;
    std::fs::create_dir_all(&gocache)
        .map_err(|e| AkError::ConfigError(format!("mkdir gocache: {e}")))?;

    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if !matches!(format, OutputFormat::Quiet) {
            eprintln!("  [{}/{}] go list -m -versions {name}", i + 1, names.len());
        }
        let versions = go_list_versions(name, upstream, work, &gomodcache, &gocache)?;
        if versions.is_empty() {
            return Err(AkError::ConfigError(format!(
                "go list -m -versions {name} returned no versions"
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

fn go_list_versions(
    module: &str,
    upstream: &UpstreamConfig,
    work: &Path,
    gomodcache: &Path,
    gocache: &Path,
) -> Result<Vec<String>> {
    let mut cmd = Command::new("go");
    cmd.args(["list", "-m", "-versions", module])
        .env("GOMODCACHE", gomodcache)
        .env("GOCACHE", gocache)
        .env("GOPROXY", upstream.goproxy())
        .env("GOSUMDB", upstream.gosumdb());
    super::engine::apply_isolated_temp(&mut cmd, work);
    let output = cmd
        .output()
        .map_err(|e| AkError::ConfigError(format!("Failed to spawn go list: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AkError::ConfigError(format!(
            "go list -m -versions {module} failed:\n{stderr}"
        ))
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // "path v1.0.0 v1.1.0 …" — first field is module path
    let mut parts = stdout.split_whitespace();
    let _path = parts.next();
    Ok(parts.map(|s| s.to_string()).collect())
}

fn encode_go_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 2);
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn resolve_go_roots(input: &Path) -> Result<Vec<RootSpec>> {
    let text = std::fs::read_to_string(input)
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", input.display())))?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == "go.mod" || text.trim_start().starts_with("module ") {
        Ok(parse_go_mod_requires(&text))
    } else {
        Ok(parse_module_list(&text))
    }
}

/// Collect direct `require` entries (skip `indirect`).
pub fn parse_go_mod_requires(text: &str) -> Vec<RootSpec> {
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block {
            if line == ")" {
                in_block = false;
                continue;
            }
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if line.contains("// indirect") {
                continue;
            }
            let cleaned = line.split("//").next().unwrap_or(line).trim();
            let mut parts = cleaned.split_whitespace();
            if let (Some(name), Some(ver)) = (parts.next(), parts.next()) {
                out.push(RootSpec {
                    name: name.to_string(),
                    version: Some(ver.to_string()),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("require ") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                continue;
            }
            if rest.contains("// indirect") {
                continue;
            }
            let cleaned = rest.split("//").next().unwrap_or(rest).trim();
            let mut parts = cleaned.split_whitespace();
            if let (Some(name), Some(ver)) = (parts.next(), parts.next()) {
                out.push(RootSpec {
                    name: name.to_string(),
                    version: Some(ver.to_string()),
                });
            }
        }
    }
    out
}

fn snapshot_versions(download_root: &Path) -> Result<HashSet<(String, String)>> {
    let mut set = HashSet::new();
    if !download_root.is_dir() {
        return Ok(set);
    }
    let modules = crate::commands::go_proxy::scan_go_proxy_cache(download_root)?;
    for m in modules {
        set.insert((m.module_encoded, m.version));
    }
    Ok(set)
}

fn sync_download_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(src, dst)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| AkError::ConfigError(format!("mkdir {}: {e}", dst.display())))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| AkError::ConfigError(format!("read {}: {e}", src.display())))?
        .filter_map(|e| e.ok())
    {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = entry
            .metadata()
            .map_err(|e| AkError::ConfigError(format!("stat {}: {e}", from.display())))?;
        if meta.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if meta.is_file() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::copy(&from, &to).map_err(|e| {
                AkError::ConfigError(format!("copy {} -> {}: {e}", from.display(), to.display()))
            })?;
        }
    }
    Ok(())
}

fn resolve_selected_version(download_root: &Path, module_encoded: &str) -> String {
    let vdir = download_root
        .join(module_encoded.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join("@v");
    let Ok(rd) = std::fs::read_dir(&vdir) else {
        return String::new();
    };
    let mut versions: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("mod") {
                p.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    versions.sort();
    versions.pop().unwrap_or_default()
}

fn backfill_manifest_from_tree(
    payload: &Path,
    ferry_download: &Path,
    manifest: &mut FerryManifest,
) -> Result<()> {
    let modules = crate::commands::go_proxy::scan_go_proxy_cache(ferry_download)?;
    for m in modules {
        let name = crate::commands::go_proxy::decode_go_path(&m.module_encoded);
        if manifest
            .modules
            .iter()
            .any(|e| e.name == name && e.version == m.version)
        {
            continue;
        }
        let mut files = Vec::new();
        for p in [&m.zip_path, &m.mod_path] {
            if p.is_file() {
                files.push(file_entry(payload, p)?);
            }
        }
        let info = m.zip_path.with_extension("info");
        if info.is_file() {
            files.push(file_entry(payload, &info)?);
        }
        manifest.record_module(
            payload,
            ModuleEntry {
                name,
                name_encoded: Some(m.module_encoded),
                version: m.version,
                files,
                via: "scan".into(),
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_go_mod_direct_only() {
        let text = r#"
module example.com/app

go 1.22

require (
        github.com/foo/bar v1.2.3
        golang.org/x/sync v0.1.0 // indirect
)

require github.com/solo/pkg v0.9.0
"#;
        let roots = parse_go_mod_requires(text);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].name, "github.com/foo/bar");
        assert_eq!(roots[1].name, "github.com/solo/pkg");
    }
}
