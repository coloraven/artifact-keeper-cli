//! Zip a ferry staging directory into the output archive.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use miette::Result;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::error::AkError;

/// Create `output` zip from all files under `staging` (relative paths preserved).
pub fn zip_dir(staging: &Path, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            AkError::ConfigError(format!("Cannot create {}: {e}", parent.display()))
        })?;
    }

    let file = File::create(output)
        .map_err(|e| AkError::ConfigError(format!("Cannot create {}: {e}", output.display())))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut files = Vec::new();
    collect_files(staging, staging, &mut files)?;
    files.sort();

    for abs in files {
        let rel = abs.strip_prefix(staging).unwrap();
        let name = rel.to_string_lossy().replace('\\', "/");
        if name.is_empty() {
            continue;
        }
        zip.start_file(&name, opts).map_err(|e| {
            AkError::ConfigError(format!("Zip start_file {name}: {e}"))
        })?;
        let mut f = File::open(&abs)
            .map_err(|e| AkError::ConfigError(format!("Open {}: {e}", abs.display())))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| AkError::ConfigError(format!("Read {}: {e}", abs.display())))?;
        zip.write_all(&buf)
            .map_err(|e| AkError::ConfigError(format!("Zip write {name}: {e}")))?;
    }

    zip.finish()
        .map_err(|e| AkError::ConfigError(format!("Zip finish {}: {e}", output.display())))?;
    Ok(())
}

/// Copy (or rename) `src` directory tree to `dst`. Does not delete `dst` if it
/// already exists; files are merged on top. Never deletes `src` unless a same-
/// volume rename succeeds and `dst` did not already exist.
pub fn copy_dir_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(AkError::ConfigError(format!(
            "Payload is not a directory: {}",
            src.display()
        ))
        .into());
    }
    if src == dst {
        return Ok(());
    }
    if dst.exists() {
        if dst.is_file() {
            return Err(AkError::ConfigError(format!(
                "Output exists as a file (need a directory for --no-archive): {}",
                dst.display()
            ))
            .into());
        }
        return copy_recursive(src, dst);
    }
    if let Some(parent) = dst.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            AkError::ConfigError(format!("Cannot create {}: {e}", parent.display()))
        })?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => copy_recursive(src, dst),
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        AkError::ConfigError(format!("Cannot create {}: {e}", dst.display()))
    })?;
    let entries = std::fs::read_dir(src).map_err(|e| {
        AkError::ConfigError(format!("Cannot read {}: {e}", src.display()))
    })?;
    for entry in entries.filter_map(|e| e.ok()) {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            copy_recursive(&from, &to)?;
        } else if meta.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                AkError::ConfigError(format!(
                    "Copy {} -> {}: {e}",
                    from.display(),
                    to.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn collect_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|e| {
        AkError::ConfigError(format!("Cannot read {}: {e}", current.display()))
    })?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            collect_files(root, &path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_roundtrip_lists_entries() {
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(staging.path().join("ak-ferry.json"), b"{}").unwrap();
        let nested = staging.path().join("download").join("x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.mod"), b"mod").unwrap();

        let out = tempfile::tempdir().unwrap();
        let zip_path = out.path().join("p.zip");
        zip_dir(staging.path(), &zip_path).unwrap();
        assert!(zip_path.is_file());

        let f = File::open(&zip_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut names = Vec::new();
        for i in 0..z.len() {
            names.push(z.by_index(i).unwrap().name().to_string());
        }
        assert!(names.iter().any(|n| n == "ak-ferry.json"));
        assert!(names.iter().any(|n| n == "download/x/a.mod"));
    }

    #[test]
    fn copy_dir_tree_preserves_layout() {
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(staging.path().join("ak-ferry.json"), b"{}").unwrap();
        let nested = staging.path().join("npm").join("lodash");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("lodash-4.17.21.tgz"), b"tgz").unwrap();

        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("ferry-out");
        copy_dir_tree(staging.path(), &dest).unwrap();
        assert!(dest.join("ak-ferry.json").is_file());
        assert!(dest.join("npm").join("lodash").join("lodash-4.17.21.tgz").is_file());
    }
}
