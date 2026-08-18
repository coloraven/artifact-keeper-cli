//! SQLite + on-disk blob store for ferry download skip / resume.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use miette::Result;
use rusqlite::{params, Connection};

use crate::error::AkError;

use super::manifest::sha256_file;

/// Schema version 2: adds `node` to cache keys (npm ABI / engines targeting).
const SCHEMA_VERSION: i32 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS downloaded (
    ecosystem TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    node TEXT NOT NULL DEFAULT '',
    sha256 TEXT,
    blob_dir TEXT NOT NULL,
    downloaded_at TEXT NOT NULL,
    PRIMARY KEY (ecosystem, name, version, target, node)
);

CREATE TABLE IF NOT EXISTS closures (
    root_ecosystem TEXT NOT NULL,
    root_name TEXT NOT NULL,
    root_version TEXT NOT NULL,
    root_target TEXT NOT NULL DEFAULT '',
    root_node TEXT NOT NULL DEFAULT '',
    dep_name TEXT NOT NULL,
    dep_version TEXT NOT NULL,
    PRIMARY KEY (root_ecosystem, root_name, root_version, root_target, root_node, dep_name, dep_version)
);
"#;

/// Identity of one cacheable download unit (one package/module version).
#[derive(Debug, Clone)]
pub struct CacheKey {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    /// npm platform label (`linux-x64`), or empty
    pub target: String,
    /// npm Node major/version label (`18`, `20.11.0`), or empty
    pub node: String,
}

impl CacheKey {
    pub fn npm(
        name: &str,
        version: &str,
        target: Option<&str>,
        node: Option<&str>,
    ) -> Self {
        Self {
            ecosystem: "npm".into(),
            name: name.to_string(),
            version: version.to_string(),
            target: target.unwrap_or("").to_string(),
            node: node.unwrap_or("").to_string(),
        }
    }

    pub fn go(name: &str, version: &str) -> Self {
        Self {
            ecosystem: "go".into(),
            name: name.to_string(),
            version: version.to_string(),
            target: String::new(),
            node: String::new(),
        }
    }

    pub fn pypi(
        name: &str,
        version: &str,
        platform: Option<&str>,
        python: Option<&str>,
    ) -> Self {
        Self {
            ecosystem: "pypi".into(),
            name: name.to_string(),
            version: version.to_string(),
            target: platform.unwrap_or("").to_string(),
            node: python.unwrap_or("").to_string(),
        }
    }

    pub fn cargo(name: &str, version: &str) -> Self {
        Self {
            ecosystem: "cargo".into(),
            name: name.to_string(),
            version: version.to_string(),
            target: String::new(),
            node: String::new(),
        }
    }
}

pub struct DownloadCache {
    conn: Mutex<Connection>,
    blob_root: PathBuf,
    force: bool,
}

impl DownloadCache {
    /// Open (or create) the cache DB. Blobs live in `<db_dir>/blobs/`.
    pub fn open(db_path: &Path, force: bool) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AkError::ConfigError(format!("Cannot create cache dir {}: {e}", parent.display()))
            })?;
        }
        let conn = Connection::open(db_path).map_err(|e| {
            AkError::ConfigError(format!("Cannot open download cache {}: {e}", db_path.display()))
        })?;
        migrate(&conn)?;
        let blob_root = db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("blobs");
        std::fs::create_dir_all(&blob_root).map_err(|e| {
            AkError::ConfigError(format!("Cannot create blob dir {}: {e}", blob_root.display()))
        })?;
        Ok(Self {
            conn: Mutex::new(conn),
            blob_root,
            force,
        })
    }

    pub fn force(&self) -> bool {
        self.force
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| AkError::ConfigError(format!("download cache lock poisoned: {e}")).into())
    }

    /// True when we should skip network fetch (cached and not --force).
    pub fn should_skip(&self, key: &CacheKey) -> Result<bool> {
        if self.force {
            return Ok(false);
        }
        self.contains(key)
    }

    pub fn contains(&self, key: &CacheKey) -> Result<bool> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT 1 FROM downloaded
                 WHERE ecosystem=?1 AND name=?2 AND version=?3 AND target=?4 AND node=?5",
            )
            .map_err(|e| AkError::ConfigError(format!("cache prepare: {e}")))?;
        let found = stmt
            .exists(params![
                key.ecosystem,
                key.name,
                key.version,
                key.target,
                key.node
            ])
            .map_err(|e| AkError::ConfigError(format!("cache query: {e}")))?;
        drop(stmt);
        drop(conn);
        if !found {
            return Ok(false);
        }
        if !self.verify_blobs(key)? {
            // Corrupt / truncated / interrupted store — drop so the next fetch re-downloads.
            let _ = self.invalidate(key);
            return Ok(false);
        }
        Ok(true)
    }

    /// Drop the SQLite row and on-disk blob directory for `key`.
    pub fn invalidate(&self, key: &CacheKey) -> Result<()> {
        let blob = self.blob_dir_for(key);
        if blob.exists() {
            let _ = std::fs::remove_dir_all(&blob);
        }
        self.lock_conn()?
            .execute(
                "DELETE FROM downloaded
                 WHERE ecosystem=?1 AND name=?2 AND version=?3 AND target=?4 AND node=?5",
                params![
                    key.ecosystem,
                    key.name,
                    key.version,
                    key.target,
                    key.node
                ],
            )
            .map_err(|e| AkError::ConfigError(format!("cache invalidate: {e}")))?;
        Ok(())
    }

    fn stored_sha256(&self, key: &CacheKey) -> Result<Option<String>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT sha256 FROM downloaded
                 WHERE ecosystem=?1 AND name=?2 AND version=?3 AND target=?4 AND node=?5",
            )
            .map_err(|e| AkError::ConfigError(format!("cache prepare sha: {e}")))?;
        let mut rows = stmt
            .query(params![
                key.ecosystem,
                key.name,
                key.version,
                key.target,
                key.node
            ])
            .map_err(|e| AkError::ConfigError(format!("cache query sha: {e}")))?;
        match rows.next() {
            Ok(Some(row)) => {
                let sha: Option<String> = row
                    .get(0)
                    .map_err(|e| AkError::ConfigError(format!("cache sha row: {e}")))?;
                Ok(sha.filter(|s| !s.is_empty()))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(AkError::ConfigError(format!("cache sha next: {e}")).into()),
        }
    }

    /// True when blob files look complete (non-empty, no `*.partial`, sha matches when stored).
    fn verify_blobs(&self, key: &CacheKey) -> Result<bool> {
        let blob = self.blob_dir_for(key);
        if !blob.is_dir() {
            return Ok(false);
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&blob)
            .map_err(|e| AkError::ConfigError(format!("read {}: {e}", blob.display())))?
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if name.ends_with(".partial") {
                return Ok(false);
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return Ok(false),
            };
            if meta.len() == 0 {
                return Ok(false);
            }
            files.push(path);
        }
        if files.is_empty() {
            return Ok(false);
        }
        if let Some(expected) = self.stored_sha256(key)? {
            let mut matched = false;
            for path in &files {
                let (actual, _) = sha256_file(path)?;
                if actual == expected {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn blob_dir_for(&self, key: &CacheKey) -> PathBuf {
        let mut p = self.blob_root.join(&key.ecosystem);
        for part in key.name.split('/') {
            p.push(sanitize_seg(part));
        }
        if !key.target.is_empty() {
            p.push(sanitize_seg(&key.target));
        }
        if !key.node.is_empty() {
            p.push(format!("node-{}", sanitize_seg(&key.node)));
        }
        p.push(sanitize_seg(&key.version));
        p
    }

    /// Copy cached files into `dest_dir` (must exist). Returns absolute paths written.
    /// Existing dest files are kept only when their sha256 matches the cached blob.
    pub fn materialize(&self, key: &CacheKey, dest_dir: &Path) -> Result<Vec<PathBuf>> {
        let blob = self.blob_dir_for(key);
        if !blob.is_dir() {
            return Err(AkError::ConfigError(format!(
                "Cache entry missing blobs at {}",
                blob.display()
            ))
            .into());
        }
        if !self.verify_blobs(key)? {
            let _ = self.invalidate(key);
            return Err(AkError::ConfigError(format!(
                "Cache entry corrupt at {}",
                blob.display()
            ))
            .into());
        }
        std::fs::create_dir_all(dest_dir).map_err(|e| {
            AkError::ConfigError(format!("Cannot create {}: {e}", dest_dir.display()))
        })?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&blob)
            .map_err(|e| AkError::ConfigError(format!("read {}: {e}", blob.display())))?
            .filter_map(|e| e.ok())
        {
            let from = entry.path();
            if !from.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".partial") {
                continue;
            }
            let to = dest_dir.join(&name);
            if to.is_file() {
                let (sha_to, _) = sha256_file(&to)?;
                let (sha_from, _) = sha256_file(&from)?;
                if sha_to == sha_from {
                    out.push(to);
                    continue;
                }
                let _ = std::fs::remove_file(&to);
            }
            std::fs::copy(&from, &to).map_err(|e| {
                AkError::ConfigError(format!(
                    "cache restore {} -> {}: {e}",
                    from.display(),
                    to.display()
                ))
            })?;
            out.push(to);
        }
        if out.is_empty() {
            return Err(AkError::ConfigError(format!(
                "Cache blob dir empty: {}",
                blob.display()
            ))
            .into());
        }
        Ok(out)
    }

    /// Persist files into the blob store and upsert the SQLite row.
    pub fn store(&self, key: &CacheKey, files: &[PathBuf]) -> Result<()> {
        let blob = self.blob_dir_for(key);
        if blob.exists() {
            std::fs::remove_dir_all(&blob).ok();
        }
        std::fs::create_dir_all(&blob).map_err(|e| {
            AkError::ConfigError(format!("Cannot create {}: {e}", blob.display()))
        })?;

        let mut primary_sha = None;
        for src in files {
            if !src.is_file() {
                continue;
            }
            let name = src
                .file_name()
                .ok_or_else(|| AkError::ConfigError(format!("Bad file {}", src.display())))?;
            let dest = blob.join(name);
            std::fs::copy(src, &dest).map_err(|e| {
                AkError::ConfigError(format!(
                    "cache store {} -> {}: {e}",
                    src.display(),
                    dest.display()
                ))
            })?;
            if primary_sha.is_none() {
                let (sha, _) = sha256_file(src)?;
                primary_sha = Some(sha);
            }
        }

        let rel_blob = blob
            .strip_prefix(&self.blob_root)
            .unwrap_or(&blob)
            .to_string_lossy()
            .replace('\\', "/");
        let now = Utc::now().to_rfc3339();
        self.lock_conn()?
            .execute(
                "INSERT INTO downloaded
                   (ecosystem, name, version, target, node, sha256, blob_dir, downloaded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(ecosystem, name, version, target, node) DO UPDATE SET
                   sha256=excluded.sha256,
                   blob_dir=excluded.blob_dir,
                   downloaded_at=excluded.downloaded_at",
                params![
                    key.ecosystem,
                    key.name,
                    key.version,
                    key.target,
                    key.node,
                    primary_sha,
                    rel_blob,
                    now,
                ],
            )
            .map_err(|e| AkError::ConfigError(format!("cache upsert: {e}")))?;
        Ok(())
    }

    /// Record that `deps` were pulled while resolving `root`.
    pub fn store_closure(&self, root: &CacheKey, deps: &[(String, String)]) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM closures
             WHERE root_ecosystem=?1 AND root_name=?2 AND root_version=?3
               AND root_target=?4 AND root_node=?5",
            params![
                root.ecosystem,
                root.name,
                root.version,
                root.target,
                root.node
            ],
        )
        .map_err(|e| AkError::ConfigError(format!("cache closure clear: {e}")))?;
        for (name, version) in deps {
            conn.execute(
                "INSERT OR IGNORE INTO closures
                 (root_ecosystem, root_name, root_version, root_target, root_node,
                  dep_name, dep_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    root.ecosystem,
                    root.name,
                    root.version,
                    root.target,
                    root.node,
                    name,
                    version
                ],
            )
            .map_err(|e| AkError::ConfigError(format!("cache closure insert: {e}")))?;
        }
        Ok(())
    }

    /// Root + recorded transitive deps (name, version). Root itself is included.
    pub fn closure_members(&self, root: &CacheKey) -> Result<Vec<(String, String)>> {
        let conn = self.lock_conn()?;
        let mut out = vec![(root.name.clone(), root.version.clone())];
        let mut stmt = conn
            .prepare(
                "SELECT dep_name, dep_version FROM closures
                 WHERE root_ecosystem=?1 AND root_name=?2 AND root_version=?3
                   AND root_target=?4 AND root_node=?5",
            )
            .map_err(|e| AkError::ConfigError(format!("cache closure prepare: {e}")))?;
        let rows = stmt
            .query_map(
                params![
                    root.ecosystem,
                    root.name,
                    root.version,
                    root.target,
                    root.node
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| AkError::ConfigError(format!("cache closure query: {e}")))?;
        for row in rows {
            let pair = row.map_err(|e| AkError::ConfigError(format!("cache closure row: {e}")))?;
            if !out.iter().any(|(n, v)| n == &pair.0 && v == &pair.1) {
                out.push(pair);
            }
        }
        Ok(out)
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let ver: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap_or(0);
    if ver < SCHEMA_VERSION {
        // Breaking key change (add node): rebuild empty schema. Blobs on disk may linger.
        conn.execute_batch(
            "DROP TABLE IF EXISTS downloaded;
             DROP TABLE IF EXISTS closures;",
        )
        .map_err(|e| AkError::ConfigError(format!("cache migrate drop: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| AkError::ConfigError(format!("cache migrate schema: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| AkError::ConfigError(format!("cache migrate version: {e}")))?;
    } else {
        conn.execute_batch(SCHEMA)
            .map_err(|e| AkError::ConfigError(format!("Cannot init download cache schema: {e}")))?;
    }
    Ok(())
}

fn sanitize_seg(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

/// Default cache DB path under the AK config directory.
pub fn default_cache_db_path() -> Result<PathBuf> {
    let dir = crate::config::config_dir()?.join("ferry-cache");
    Ok(dir.join("downloads.sqlite"))
}

/// Normalize a user Node label for npm/node-gyp (`18` → `18.0.0`).
pub fn normalize_node_target(raw: &str) -> String {
    let s = raw.trim().trim_start_matches('v');
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        format!("{s}.0.0")
    } else {
        s.to_string()
    }
}

pub fn parse_node_list(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in raw {
        for part in item.split(',') {
            let part = part.trim().trim_start_matches('v');
            if part.is_empty() {
                continue;
            }
            if !seen.insert(part.to_string()) {
                continue;
            }
            // light validation
            if part.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
                return Err(AkError::ConfigError(format!(
                    "Invalid --node '{part}' (expected major like 18 or version like 20.11.0)"
                ))
                .into());
            }
            out.push(part.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_store_materialize_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("downloads.sqlite");
        let cache = DownloadCache::open(&db, false).unwrap();

        let key = CacheKey::npm("lodash", "4.17.21", Some("linux-x64"), Some("20"));
        assert!(!cache.should_skip(&key).unwrap());

        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let tgz = src_dir.join("lodash-4.17.21.tgz");
        std::fs::write(&tgz, b"tarball").unwrap();
        cache.store(&key, &[tgz]).unwrap();

        assert!(cache.should_skip(&key).unwrap());
        // Different node → miss
        let other = CacheKey::npm("lodash", "4.17.21", Some("linux-x64"), Some("18"));
        assert!(!cache.should_skip(&other).unwrap());

        let dest = tmp.path().join("out");
        let files = cache.materialize(&key, &dest).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(files[0].as_path()).unwrap(), b"tarball");

        let forced = DownloadCache::open(&db, true).unwrap();
        assert!(!forced.should_skip(&key).unwrap());
    }

    #[test]
    fn normalize_and_parse_nodes() {
        assert_eq!(normalize_node_target("18"), "18.0.0");
        assert_eq!(normalize_node_target("v20.11.0"), "20.11.0");
        let nodes = parse_node_list(&["18,20".into(), "18".into()]).unwrap();
        assert_eq!(nodes, vec!["18", "20"]);
    }

    #[test]
    fn corrupt_blob_invalidates_contains() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("downloads.sqlite");
        let cache = DownloadCache::open(&db, false).unwrap();
        let key = CacheKey::cargo("serde", "1.0.0");
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let crate_path = src_dir.join("serde-1.0.0.crate");
        std::fs::write(&crate_path, b"good-bytes").unwrap();
        cache.store(&key, &[crate_path]).unwrap();
        assert!(cache.should_skip(&key).unwrap());

        // Truncate the cached blob to simulate a crash mid-write.
        let blob = cache.blob_dir_for(&key);
        let cached_file = std::fs::read_dir(&blob)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&cached_file, b"bad").unwrap();
        assert!(!cache.should_skip(&key).unwrap());
        assert!(!cache.contains(&key).unwrap());
    }

    #[test]
    fn partial_suffix_rejects_contains() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("downloads.sqlite");
        let cache = DownloadCache::open(&db, false).unwrap();
        let key = CacheKey::cargo("bitflags", "2.5.0");
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let crate_path = src_dir.join("bitflags-2.5.0.crate");
        std::fs::write(&crate_path, b"crate-bytes").unwrap();
        cache.store(&key, &[crate_path]).unwrap();

        let blob = cache.blob_dir_for(&key);
        std::fs::write(blob.join("leftover.crate.partial"), b"xx").unwrap();
        assert!(!cache.should_skip(&key).unwrap());
    }
}
