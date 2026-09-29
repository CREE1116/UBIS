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
    /// Message body (for a merge: usually the pull request title/text).
    pub body: String,
    /// A merge commit; its hunks are the diff against the first parent,
    /// i.e. the whole merged change.
    pub merge: bool,
    pub hunks: Vec<Hunk>,
    /// Post-commit blob of each changed path (deleted paths omitted).
    pub blobs: Vec<(String, String)>,
}

impl Commit {
    /// Post-commit blob id of `path`, if the commit changed it.
    pub fn blob(&self, path: &str) -> Option<&str> {
        self.blobs.iter().find(|(p, _)| p == path).map(|(_, b)| b.as_str())
    }

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
/// with their hunks. A merge is recorded as one change against its first
/// parent (a merged pull request as a unit, with its message).
pub fn history(repo: &Path, rev: &str, max_commits: usize) -> Result<Vec<Commit>> {
    let n = format!("-n{max_commits}");
    let raw = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--diff-merges=first-parent",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--raw",
            "--no-abbrev",
            "--unified=0",
            "--format=%x1e%H%x1f%ct%x1f%P%x1f%s%x1f%b%x1d",
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
        // `id ␟ time ␟ parents ␟ subject ␟ body ␝ raw/patch lines`
        let (header, rest) = record.split_once('\u{1d}').unwrap_or((record, ""));
        let lines = rest.lines();
        let mut parts = header.splitn(5, '\u{1f}');
        let id = parts.next().unwrap_or("").trim().to_string();
        let ts = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let merge = parts.next().unwrap_or("").split_whitespace().count() > 1;
        let subject = parts.next().unwrap_or("").to_string();
        let body = parts.next().unwrap_or("").trim().to_string();
        let mut hunks = Vec::new();
        let mut blobs = Vec::new();
        let mut current: Option<String> = None;
        for line in lines {
            if let Some(raw) = line.strip_prefix(':') {
                // `:old_mode new_mode old_sha new_sha STATUS\tpath[\tnew_path]`
                let (meta, paths) = raw.split_once('\t').unwrap_or((raw, ""));
                let new_sha = meta.split_whitespace().nth(3).unwrap_or("");
                let path = paths.rsplit('\t').next().unwrap_or("");
                if !path.is_empty() && !new_sha.is_empty() && new_sha.bytes().any(|b| b != b'0') {
                    blobs.push((path.to_string(), new_sha.to_string()));
                }
            } else if line.starts_with("diff --git ") {
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
                body,
                merge,
                hunks,
                blobs,
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

/// Reads many objects through one `git cat-file --batch` process instead of
/// spawning `git show` per file.
pub struct BlobReader {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl BlobReader {
    pub fn new(repo: &Path) -> Result<Self> {
        use std::process::Stdio;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("git cat-file --batch")?;
        let stdin = child.stdin.take().context("cat-file stdin")?;
        let stdout = std::io::BufReader::new(child.stdout.take().context("cat-file stdout")?);
        Ok(Self { child, stdin, stdout })
    }

    /// Contents of an object (`<sha>` or `<rev>:<path>`), `None` if missing
    /// or not valid UTF-8.
    pub fn read(&mut self, object: &str) -> Result<Option<String>> {
        use std::io::{BufRead, Read, Write};
        writeln!(self.stdin, "{object}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let mut parts = header.split_whitespace();
        let (_, kind, size) = (parts.next(), parts.next(), parts.next());
        let Some(size) = size.and_then(|s| s.parse::<usize>().ok()) else {
            return Ok(None); // "<object> missing"
        };
        let mut buf = vec![0u8; size + 1]; // content + trailing newline
        self.stdout.read_exact(&mut buf)?;
        buf.truncate(size);
        if kind != Some("blob") {
            return Ok(None);
        }
        Ok(String::from_utf8(buf).ok())
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The change from `base` to `head` as one pseudo-commit (id and time of
/// `head`), e.g. a whole pull request.
pub fn diff(repo: &Path, base: &str, head: &str) -> Result<Commit> {
    let raw = git(
        repo,
        &["diff", "--no-color", "--no-ext-diff", "-M", "--raw", "--no-abbrev", "--unified=0", base, head],
    )?;
    let ts = git(repo, &["show", "-s", "--format=%ct", head])?;
    let record = format!("\u{1e}{head}\u{1f}{}\u{1f}\u{1f}\u{1f}\u{1d}\n{raw}", ts.trim());
    parse_log(&record).pop().context("empty diff record")
}

/// Root of the work tree containing `dir`.
pub fn toplevel(dir: &Path) -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?.trim()))
}

/// Full commit id of `rev`.
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    Ok(git(repo, &["rev-parse", "--verify", rev])?.trim().to_string())
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
        let raw = "\u{1e}abc\u{1f}100\u{1f}p1 p2\u{1f}Fix thing\u{1f}Body line\nmore\u{1d}\n:100644 100644 1111 2222 M\tsrc/a.rs\n:100644 000000 3333 0000 D\told.md\ndiff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn x\n+a\n+b\n@@ -10 +12 @@\n-x\n+y\n@@ -20,3 +21,0 @@\n-gone\ndiff --git a/old.md b/old.md\n--- a/old.md\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n";
        let c = parse_log(raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "Fix thing");
        assert_eq!(c[0].body, "Body line\nmore");
        assert!(c[0].merge);
        assert_eq!(c[0].blobs, vec![("src/a.rs".to_string(), "2222".to_string())]);
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
