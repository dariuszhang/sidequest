use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use anyhow::Result;
use walkdir::WalkDir;

use crate::harvester::run_git;

#[derive(Debug, Default)]
pub(super) struct BuildScan {
    pub(super) todo_hits: usize,
    pub(super) missing_files: Vec<String>,
    pub(super) examples: Vec<String>,
}

pub(super) fn scan_build_opportunities(path: &Path) -> Result<BuildScan> {
    let mut scan = BuildScan::default();
    for required in ["README.md", "Dockerfile", ".github/workflows"] {
        if !path.join(required).exists() {
            scan.missing_files.push(required.to_string());
        }
    }

    for entry in WalkDir::new(path)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_scan_entry)
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_scan_candidate(entry.path()) {
            continue;
        }
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for (line_number, line) in contents.lines().enumerate() {
            if line.contains("TODO") || line.contains("FIXME") || line.contains("HACK") {
                scan.todo_hits += 1;
                if scan.examples.len() < 5 {
                    scan.examples.push(format!(
                        "{}:{} - {}",
                        entry.path().display(),
                        line_number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    Ok(scan)
}

pub(super) fn repo_can_accept_sidequest_run(path: &Path) -> Result<bool> {
    let output = run_git(path, ["status", "--porcelain"])?;
    Ok(!output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .any(|line| !status_entry_is_sidequest_only(line)))
}

pub(super) fn quoted_commit_log(commits: &str) -> String {
    let mut output = String::new();
    for line in commits.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.splitn(2, ' ');
        let hash = parts.next().unwrap_or_default();
        let subject = parts.next().unwrap_or_default().trim();
        if !hash.is_empty() {
            let escaped_subject = subject.replace('\\', "\\\\").replace('"', "\\\"");
            output.push_str(&format!("- `{hash}` \"{escaped_subject}\"\n"));
        }
    }
    if output.is_empty() {
        output.push_str("- none recorded\n");
    }
    output
}

const SKIP_SCAN_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "vendor",
    ".venv",
    "venv",
    ".cache",
    "coverage",
    "build",
];

pub(super) const MAX_SCAN_FILE_BYTES: u64 = 256 * 1024;
const MAX_SCAN_SAMPLE_BYTES: usize = 8 * 1024;

fn should_scan_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }

    let name = entry.file_name();
    !SKIP_SCAN_DIRECTORIES
        .iter()
        .any(|skip| name == std::ffi::OsStr::new(skip))
}

fn is_scan_candidate(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() > MAX_SCAN_FILE_BYTES {
        return false;
    }

    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut sample = [0u8; MAX_SCAN_SAMPLE_BYTES];
    let Ok(bytes_read) = file.read(&mut sample) else {
        return false;
    };
    if bytes_read == 0 {
        return false;
    }
    let sample = &sample[..bytes_read];
    !sample.contains(&0) && std::str::from_utf8(sample).is_ok()
}

fn status_entry_is_sidequest_only(line: &str) -> bool {
    // Porcelain v1 entries are `XY <path>` or `XY <old> -> <new>`.
    let path_field = line.get(3..).unwrap_or(line).trim();
    if path_field.is_empty() {
        return false;
    }
    if let Some((from, to)) = path_field.split_once(" -> ") {
        return path_is_sidequest_metadata(from) && path_is_sidequest_metadata(to);
    }
    path_is_sidequest_metadata(path_field)
}

fn path_is_sidequest_metadata(path: &str) -> bool {
    let candidate = unquote_porcelain_path(path);
    candidate == ".sidequest" || candidate.starts_with(".sidequest/")
}

fn unquote_porcelain_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        trimmed.to_string()
    }
}
