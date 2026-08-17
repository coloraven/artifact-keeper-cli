//! Go module proxy cache scan + protocol upload (`PUT /go/{repo}/{module}/@v/{version}.{zip,mod}`).

use std::path::{Path, PathBuf};

use miette::Result;

use crate::error::AkError;
use crate::output::OutputFormat;

/// One module version discovered in a Go module download / proxy cache tree.
#[derive(Debug, Clone)]
pub struct GoModuleVersion {
    /// Case-encoded module path as used by the Go proxy protocol / on-disk cache
    /// (e.g. `github.com/!data-!dog/dd-trace-go/v2`).
    pub module_encoded: String,
    pub version: String,
    pub zip_path: PathBuf,
    pub mod_path: PathBuf,
}

/// Decode Go's path encoding (`!a` → `A`) for display.
pub fn decode_go_path(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        if c == '!' {
            if let Some(n) = chars.next() {
                out.extend(n.to_uppercase());
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Encode a module path for the Go proxy protocol.
#[cfg(test)]
pub fn encode_go_path(path: &str) -> String {
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

/// True when `root` looks like a Go module proxy / download cache
/// (`<module>/@v/<version>.zip` + sibling `.mod`).
pub fn looks_like_go_proxy_cache(root: &Path) -> bool {
    !scan_go_proxy_cache(root).unwrap_or_default().is_empty()
}

/// Scan a directory tree for Go module zip+mod pairs under `@v/`.
pub fn scan_go_proxy_cache(root: &Path) -> Result<Vec<GoModuleVersion>> {
    let mut out = Vec::new();
    scan_go_proxy_cache_rec(root, root, &mut out)?;
    out.sort_by(|a, b| {
        (&a.module_encoded, &a.version).cmp(&(&b.module_encoded, &b.version))
    });
    Ok(out)
}

fn scan_go_proxy_cache_rec(
    root: &Path,
    current: &Path,
    out: &mut Vec<GoModuleVersion>,
) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|e| {
        AkError::ConfigError(format!("Cannot read {}: {e}", current.display()))
    })?;

    let mut dirs = Vec::new();
    let mut zips = Vec::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            dirs.push(path);
        } else if meta.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("zip") {
                zips.push(path);
            }
        }
    }

    // Only treat as module versions when parent dir is named `@v`.
    if current.file_name().and_then(|n| n.to_str()) == Some("@v") {
        for zip in zips {
            let version = zip
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if version.is_empty() {
                continue;
            }
            let mod_path = current.join(format!("{version}.mod"));
            if !mod_path.is_file() {
                continue;
            }
            let module_dir = current.parent().ok_or_else(|| {
                AkError::ConfigError(format!("Invalid @v path: {}", current.display()))
            })?;
            let rel = module_dir.strip_prefix(root).map_err(|_| {
                AkError::ConfigError(format!(
                    "Path {} is not under {}",
                    module_dir.display(),
                    root.display()
                ))
            })?;
            let module_encoded = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if module_encoded.is_empty() {
                continue;
            }
            out.push(GoModuleVersion {
                module_encoded,
                version,
                zip_path: zip,
                mod_path,
            });
        }
    }

    dirs.sort();
    for dir in dirs {
        scan_go_proxy_cache_rec(root, &dir, out)?;
    }
    Ok(())
}

fn go_module_url(
    base_url: &str,
    repo: &str,
    module_encoded: &str,
    version: &str,
    suffix: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url)
        .map_err(|e| AkError::ConfigError(format!("Invalid instance URL {base_url}: {e}")))?;
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|_| AkError::ConfigError(format!("Invalid instance URL: {base_url}")))?;
        segs.pop_if_empty()
            .push("go")
            .push(repo);
        for part in module_encoded.split('/') {
            if !part.is_empty() {
                segs.push(part);
            }
        }
        segs.push("@v").push(&format!("{version}.{suffix}"));
    }
    Ok(url)
}

/// Return true if `{version}.mod` already exists on the server (version present).
pub async fn go_version_exists(
    base_url: &str,
    auth_header: &str,
    repo: &str,
    module_encoded: &str,
    version: &str,
) -> Result<bool> {
    let url = go_module_url(base_url, repo, module_encoded, version, "mod")?;
    let resp = super::client::raw_http_client()?
        .head(url)
        .header(reqwest::header::AUTHORIZATION, auth_header)
        .send()
        .await
        .map_err(|e| AkError::NetworkError(format!("Go module HEAD failed: {e}")))?;
    Ok(resp.status().is_success())
}

async fn put_go_bytes(
    base_url: &str,
    auth_header: &str,
    repo: &str,
    module_encoded: &str,
    version: &str,
    suffix: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<()> {
    let url = go_module_url(base_url, repo, module_encoded, version, suffix)?;
    let resp = super::client::raw_http_client()?
        .put(url)
        .header(reqwest::header::AUTHORIZATION, auth_header)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(|e| AkError::NetworkError(format!("Go module PUT failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AkError::ServerError(format!(
            "Go module upload failed ({status}): {text}"
        ))
        .into());
    }
    Ok(())
}

/// Upload one Go module version (zip + mod) via the proxy protocol.
pub async fn push_go_module(
    base_url: &str,
    auth_header: &str,
    repo: &str,
    module: &GoModuleVersion,
) -> Result<()> {
    let zip_bytes = tokio::fs::read(&module.zip_path)
        .await
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", module.zip_path.display())))?;
    let mod_bytes = tokio::fs::read(&module.mod_path)
        .await
        .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", module.mod_path.display())))?;

    put_go_bytes(
        base_url,
        auth_header,
        repo,
        &module.module_encoded,
        &module.version,
        "zip",
        "application/zip",
        zip_bytes,
    )
    .await?;
    put_go_bytes(
        base_url,
        auth_header,
        repo,
        &module.module_encoded,
        &module.version,
        "mod",
        "text/plain",
        mod_bytes,
    )
    .await?;
    Ok(())
}

/// Push all Go modules found under `root` using the Go proxy protocol.
pub async fn push_go_proxy_cache(
    base_url: &str,
    auth_header: &str,
    repo: &str,
    root: &Path,
    skip_dupe_uploads: bool,
    format: &OutputFormat,
) -> Result<(usize, usize)> {
    let modules = scan_go_proxy_cache(root)?;
    if modules.is_empty() {
        return Err(AkError::ConfigError(format!(
            "No Go module @v/*.zip+.mod pairs under {}",
            root.display()
        ))
        .into());
    }

    let mut uploaded = 0usize;
    let mut skipped = 0usize;

    for module in &modules {
        let display = format!(
            "{}@{}",
            decode_go_path(&module.module_encoded),
            module.version
        );

        if skip_dupe_uploads
            && go_version_exists(
                base_url,
                auth_header,
                repo,
                &module.module_encoded,
                &module.version,
            )
            .await?
        {
            skipped += 1;
            if !matches!(format, OutputFormat::Quiet) {
                eprintln!("  skip dupe: {display}");
            }
            continue;
        }

        let spinner = crate::output::spinner(&format!("Uploading {display} (go protocol)..."));
        let result = push_go_module(base_url, auth_header, repo, module).await;
        spinner.finish_and_clear();
        result?;

        uploaded += 1;
        if matches!(format, OutputFormat::Quiet) {
            println!("{display}");
        } else {
            eprintln!("  {display} -> go/{repo}/...");
        }
    }

    Ok((uploaded, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_encode_roundtrip() {
        let raw = "github.com/DataDog/dd-trace-go/v2";
        let enc = encode_go_path(raw);
        assert!(enc.contains("!d"));
        assert_eq!(decode_go_path(&enc), raw);
    }

    #[test]
    fn scan_finds_zip_mod_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let vdir = tmp
            .path()
            .join("github.com")
            .join("!foo")
            .join("bar")
            .join("@v");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("v1.2.3.zip"), b"zip").unwrap();
        std::fs::write(vdir.join("v1.2.3.mod"), b"module github.com/Foo/bar\n").unwrap();
        std::fs::write(vdir.join("v1.2.3.info"), b"{}").unwrap();
        std::fs::write(vdir.join("list"), b"v1.2.3\n").unwrap();

        let found = scan_go_proxy_cache(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].module_encoded, "github.com/!foo/bar");
        assert_eq!(found[0].version, "v1.2.3");
        assert!(looks_like_go_proxy_cache(tmp.path()));
    }

    #[test]
    fn scan_ignores_plain_trees() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.whl"), b"x").unwrap();
        assert!(scan_go_proxy_cache(tmp.path()).unwrap().is_empty());
        assert!(!looks_like_go_proxy_cache(tmp.path()));
    }

    #[test]
    fn go_module_url_builds_path() {
        let url = go_module_url(
            "http://127.0.0.1:30080",
            "go-local",
            "github.com/!foo/bar",
            "v1.0.0",
            "zip",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:30080/go/go-local/github.com/!foo/bar/@v/v1.0.0.zip"
        );
    }
}
