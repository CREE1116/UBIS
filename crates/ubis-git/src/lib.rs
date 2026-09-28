//! Git evidence: commits and the line ranges each commit touched.
//!
//! Hunks are recorded against the *post-commit* file (`+start,len`). A pure
//! deletion (`len == 0`) is recorded as touching the line where text was
//! removed, so the surrounding unit still counts as changed.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub path: String,
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub ts: i64,
    pub subject: String,
    pub hunks: Vec<Hunk>,
}

impl Commit {
    pub fn files(&self) -> Vec<&str> {
        let mut f: Vec<&str> = self.hunks.iter().map(|h| h.path.as_str()).collect();
        f.sort();
        f.dedup();
        f
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn is_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--is-inside-work-tree"]).is_ok()
}

/// Commits reachable from `rev` along first-parent history, oldest first,
/// with their hunks. Merge commits contribute no hunks of their own.
pub fn history(repo: &Path, rev: &str, max_commits: usize) -> Result<Vec<Commit>> {
    let n = format!("-n{max_commits}");
    let raw = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--no-merges",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--unified=0",
            "--format=%x1e%H%x1f%ct%x1f%s",
            "-p",
            &n,
            rev,
        ],
    )?;
    let mut commits = parse_log(&raw);
    commits.reverse();
    Ok(commits)
}

/// Parse `git log -p --unified=0` output produced with the record-separator
/// format above.
pub fn parse_log(raw: &str) -> Vec<Commit> {
    let mut out = Vec::new();
    for record in raw.split('\u{1e}').filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let header = lines.next().unwrap_or("");
        let mut parts = header.split('\u{1f}');
        let id = parts.next().unwrap_or("").to_string();
        let ts = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let subject = parts.next().unwrap_or("").to_string();
        let mut hunks = Vec::new();
        let mut current: Option<String> = None;
        for line in lines {
            if line.starts_with("diff --git ") {
                current = None;
            } else if let Some(p) = line.strip_prefix("+++ ") {
                current = if p == "/dev/null" {
                    None
                } else {
                    Some(p.strip_prefix("b/").unwrap_or(p).to_string())
                };
            } else if line.starts_with("@@ ") {
                if let (Some(path), Some((start, len))) = (&current, parse_hunk_header(line)) {
                    hunks.push(Hunk {
                        path: path.clone(),
                        start: start.max(1),
                        len: len.max(1),
                    });
                }
            }
        }
        if !id.is_empty() {
            out.push(Commit {
                id,
                ts,
                subject,
                hunks,
            });
        }
    }
    out
}

/// `@@ -a,b +c,d @@` → `(c, d)`; `d` defaults to 1.
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let plus = line.split_whitespace().find(|t| t.starts_with('+'))?;
    let spec = &plus[1..];
    let (start, len) = match spec.split_once(',') {
        Some((s, l)) => (s.parse().ok()?, l.parse().ok()?),
        None => (spec.parse().ok()?, 1),
    };
    Some((start, len))
}

/// File contents at a revision, or `None` if the path does not exist there.
pub fn show(repo: &Path, rev: &str, path: &str) -> Option<String> {
    git(repo, &["show", &format!("{rev}:{path}")]).ok()
}

/// First-parent commit IDs from `rev`, oldest first.
pub fn rev_list(repo: &Path, rev: &str) -> Result<Vec<String>> {
    let raw = git(repo, &["rev-list", "--first-parent", "--reverse", rev])?;
    Ok(raw.lines().map(str::to_string).collect())
}

/// Materialize the tree at `rev` into `dest` without touching the work tree.
pub fn export_tree(repo: &Path, rev: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", rev])
        .output()
        .context("git archive")?;
    if !archive.status.success() {
        bail!("git archive failed: {}", String::from_utf8_lossy(&archive.stderr));
    }
    let mut tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("tar")?;
    use std::io::Write;
    tar.stdin.take().unwrap().write_all(&archive.stdout)?;
    let status = tar.wait()?;
    if !status.success() {
        bail!("tar extraction failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunks() {
        let raw = "\u{1e}abc\u{1f}100\u{1f}Fix thing\n\ndiff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn x\n+a\n+b\n@@ -10 +12 @@\n-x\n+y\n@@ -20,3 +21,0 @@\n-gone\ndiff --git a/old.md b/old.md\n--- a/old.md\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n";
        let c = parse_log(raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "Fix thing");
        assert_eq!(
            c[0].hunks,
            vec![
                Hunk { path: "src/a.rs".into(), start: 4, len: 2 },
                Hunk { path: "src/a.rs".into(), start: 12, len: 1 },
                Hunk { path: "src/a.rs".into(), start: 21, len: 1 },
            ]
        );
    }
}
