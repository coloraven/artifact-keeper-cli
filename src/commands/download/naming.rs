//! Default ferry zip filenames when `-o` / job `output` is omitted.
//!
//! Pattern: `ak-ferry-{ecosystem}-{timestamp}[-{opts}][-r{n}-m{m}].zip`
//! (no input / package names in the filename).

use std::path::{Path, PathBuf};

use chrono::Utc;

use super::Ecosystem;

/// UTC timestamp suitable for filenames: `YYYYMMDDTHHMMSSmmmZ`.
pub fn ferry_timestamp_utc() -> String {
    let now = Utc::now();
    format!(
        "{}{:03}Z",
        now.format("%Y%m%dT%H%M%S"),
        now.timestamp_subsec_millis()
    )
}

pub fn ecosystem_slug(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::Go => "go",
        Ecosystem::Npm => "npm",
        Ecosystem::Pypi => "pypi",
        Ecosystem::Cargo => "cargo",
    }
}

/// Sanitize one filename segment: lowercase, keep `[a-z0-9._-]`, collapse runs of `-`.
pub fn sanitize_part(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        let mapped = match c {
            'A'..='Z' => Some(c.to_ascii_lowercase()),
            'a'..='z' | '0'..='9' | '.' | '_' => Some(c),
            _ => None,
        };
        match mapped {
            Some(ch) => {
                out.push(ch);
                prev_dash = false;
            }
            None => {
                if !prev_dash && !out.is_empty() {
                    out.push('-');
                    prev_dash = true;
                }
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Build optional opts segment from job knobs (no leading/trailing `-`).
///
/// Order: `av`, then each target, then each node/python version (prefixed `node`/`py`
/// when purely numeric), then `cat` when a server catalog was used.
pub fn opts_suffix(
    all_versions: bool,
    targets: &[String],
    nodes: &[String],
    has_catalog: bool,
    ecosystem: Ecosystem,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if all_versions {
        parts.push("av".into());
    }
    for t in targets {
        let s = sanitize_part(t);
        if !s.is_empty() {
            parts.push(s);
        }
    }
    for n in nodes {
        let s = sanitize_part(n);
        if s.is_empty() {
            continue;
        }
        // Distinguish runtime matrix from targets when the value is mostly digits.
        let tagged = if s.chars().all(|c| c.is_ascii_digit() || c == '.') {
            match ecosystem {
                Ecosystem::Npm => format!("node{s}"),
                Ecosystem::Pypi => format!("py{s}"),
                _ => s,
            }
        } else {
            s
        };
        parts.push(tagged);
    }
    if has_catalog {
        parts.push("cat".into());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("-"))
    }
}

/// Filename (no directory): `ak-ferry-{eco}-{ts}[-{opts}].zip` (without stats).
pub fn default_ferry_zip_name(
    ecosystem: Ecosystem,
    all_versions: bool,
    targets: &[String],
    nodes: &[String],
    has_catalog: bool,
) -> String {
    default_ferry_zip_name_with_ts(
        ecosystem,
        &ferry_timestamp_utc(),
        all_versions,
        targets,
        nodes,
        has_catalog,
    )
}

pub fn default_ferry_zip_name_with_ts(
    ecosystem: Ecosystem,
    timestamp: &str,
    all_versions: bool,
    targets: &[String],
    nodes: &[String],
    has_catalog: bool,
) -> String {
    let eco = ecosystem_slug(ecosystem);
    let mut name = format!("ak-ferry-{eco}-{timestamp}");
    if let Some(opts) = opts_suffix(all_versions, targets, nodes, has_catalog, ecosystem) {
        name.push('-');
        name.push_str(&opts);
    }
    name.push_str(".zip");
    name
}

/// Insert `-r{roots}-m{modules}` before `.zip` when the path looks like an auto ferry name.
pub fn append_stats(path: &Path, roots: usize, modules: usize) -> PathBuf {
    let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
        return path.to_path_buf();
    };
    if !file.starts_with("ak-ferry-") || !file.ends_with(".zip") {
        return path.to_path_buf();
    }
    let stem = &file[..file.len() - 4];
    // Avoid double-appending if already present.
    if stem.contains("-r") && stem.rsplit_once("-m").is_some() {
        // Heuristic: ends with -rN-mM
        if let Some((_, tail)) = stem.rsplit_once("-r") {
            if let Some((r, m)) = tail.split_once("-m") {
                if r.chars().all(|c| c.is_ascii_digit())
                    && m.chars().all(|c| c.is_ascii_digit())
                {
                    return path.to_path_buf();
                }
            }
        }
    }
    let new_name = format!("{stem}-r{roots}-m{modules}.zip");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(new_name),
        _ => PathBuf::from(new_name),
    }
}

/// Resolve the on-disk path for a job: explicit `output` or auto name under `output_dir`.
pub fn resolve_job_output(
    explicit: Option<&Path>,
    output_dir: &Path,
    ecosystem: Ecosystem,
    all_versions: bool,
    targets: &[String],
    nodes: &[String],
    has_catalog: bool,
) -> (PathBuf, bool) {
    if let Some(p) = explicit {
        return (p.to_path_buf(), false);
    }
    let name = default_ferry_zip_name(ecosystem, all_versions, targets, nodes, has_catalog);
    (output_dir.join(name), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_name_no_opts() {
        let n = default_ferry_zip_name_with_ts(
            Ecosystem::Go,
            "20260818T010740123Z",
            false,
            &[],
            &[],
            false,
        );
        assert_eq!(n, "ak-ferry-go-20260818T010740123Z.zip");
    }

    #[test]
    fn name_with_av_and_catalog() {
        let n = default_ferry_zip_name_with_ts(
            Ecosystem::Npm,
            "20260818T010740123Z",
            true,
            &["linux-x64".into()],
            &["20".into()],
            true,
        );
        assert_eq!(
            n,
            "ak-ferry-npm-20260818T010740123Z-av-linux-x64-node20-cat.zip"
        );
    }

    #[test]
    fn pypi_python_tag() {
        let n = default_ferry_zip_name_with_ts(
            Ecosystem::Pypi,
            "20260818T010740123Z",
            false,
            &["manylinux2014_x86_64".into()],
            &["3.11".into()],
            false,
        );
        assert_eq!(
            n,
            "ak-ferry-pypi-20260818T010740123Z-manylinux2014_x86_64-py3.11.zip"
        );
    }

    #[test]
    fn append_stats_once() {
        let p = PathBuf::from("out/ak-ferry-go-20260818T010740123Z.zip");
        let with = append_stats(&p, 1, 3);
        assert_eq!(
            with.file_name().unwrap().to_str().unwrap(),
            "ak-ferry-go-20260818T010740123Z-r1-m3.zip"
        );
        let again = append_stats(&with, 9, 9);
        assert_eq!(again, with);
    }

    #[test]
    fn sanitize_drops_path_chars() {
        assert_eq!(sanitize_part("linux/x64"), "linux-x64");
        assert_eq!(sanitize_part("@scope/pkg"), "scope-pkg");
    }
}
