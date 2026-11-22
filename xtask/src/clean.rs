//! Emptying cargo's target directories.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use thisos_report as report;

use crate::build;
use crate::cmd::{cargo, output};

/// The download caches, which take minutes to fetch again, kept unless
/// everything goes.
const CACHES: [&str; 2] = ["ovmf", "limine"];

/// The run's own logs, always kept.
const LOGS: &str = "xtask-logs";

/// The target directories to empty, each with its step's name: the root
/// workspace's and xtask's, or one when `CARGO_TARGET_DIR` or a cargo
/// config makes them the same.
pub fn dirs() -> Result<Vec<(String, PathBuf)>> {
    let root = build::target_dir()?;
    let metadata = output(cargo().args([
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--manifest-path",
        "xtask/Cargo.toml",
    ]))?;
    let metadata: Value = serde_json::from_str(&metadata).context("cargo metadata's output")?;
    let xtask = PathBuf::from(
        metadata["target_directory"]
            .as_str()
            .context("cargo metadata gave no target_directory for xtask")?,
    );
    Ok(if xtask == root {
        vec![("target".into(), root)]
    } else {
        vec![("root target".into(), root), ("xtask target".into(), xtask)]
    })
}

/// Empties each of `dirs`, a step each, but for the run's logs and, unless
/// `all`, the download caches. Each step's line shows what it freed unless
/// `summary`; the summary line always gives the total.
pub fn clean(dirs: &[(String, PathBuf)], all: bool, summary: bool) -> Result<()> {
    let mut total = 0;
    for (name, dir) in dirs {
        let freed = report::step(name, || {
            let freed = empty(dir, all)?;
            if !summary {
                report::step_detail(&format!("{} freed", human(freed)));
            }
            Ok(freed)
        })?;
        total += freed;
        report::run_detail(&format!("{} freed", human(total)));
    }
    Ok(())
}

/// Removes everything in `dir` but [`LOGS`] and, unless `all`, [`CACHES`],
/// and returns the disk space that freed.
fn empty(dir: &Path, all: bool) -> Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut doomed = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        let name = path.file_name().and_then(|name| name.to_str());
        let kept = name.is_some_and(|name| name == LOGS || (!all && CACHES.contains(&name)));
        if !kept {
            doomed.push(path);
        }
    }

    let mut seen = HashSet::new();
    let mut sizes = Vec::new();
    for path in &doomed {
        sizes.push(size(path, &mut seen)?);
    }
    let total = sizes.iter().sum();
    let mut freed = 0;
    for (path, size) in doomed.iter().zip(sizes) {
        let removed = if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        removed.with_context(|| format!("removing {}", path.display()))?;
        freed += size;
        report::bytes(freed, Some(total));
    }
    Ok(freed)
}

/// The disk space under `path`, counting a file with several links once:
/// cargo hard-links each binary it builds into the profile's directory.
fn size(path: &Path, seen: &mut HashSet<(u64, u64)>) -> Result<u64> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino()))
    {
        0
    } else {
        meta.blocks() * 512
    };
    if meta.is_dir() {
        for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            let entry = entry.with_context(|| format!("reading {}", path.display()))?;
            bytes += size(&entry.path(), seen)?;
        }
    }
    Ok(bytes)
}

/// `2.6 GiB`, `40.0 KiB`, `512 B`.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{empty, human};

    #[test]
    fn keeps_the_logs_and_the_caches_unless_told() {
        let dir = std::env::temp_dir().join(format!("xtask-clean-test-{}", std::process::id()));
        for sub in ["ovmf", "limine/12.9.6", "xtask-logs", "debug/deps", "miri"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("debug/deps/bin"), vec![1; 1 << 20]).unwrap();
        // A hard link, as cargo makes for each binary, frees nothing more.
        fs::hard_link(dir.join("debug/deps/bin"), dir.join("debug/bin")).unwrap();
        fs::write(dir.join("thisos.img"), [0; 4096]).unwrap();

        let freed = empty(&dir, false).unwrap();
        // Counting the link twice would free over 2 MiB.
        assert!((1 << 20..3 << 19).contains(&freed), "freed {freed}");
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["limine", "ovmf", "xtask-logs"]);

        empty(&dir, true).unwrap();
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["xtask-logs"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_read_as_binary_units() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(40 << 10), "40.0 KiB");
        assert_eq!(human((26 << 30) / 10), "2.6 GiB");
    }
}
