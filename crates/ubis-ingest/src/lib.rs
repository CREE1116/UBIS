//! UBIS ingest: text admission, format extractors, and the incremental indexer.

pub mod code;
pub mod history;
pub mod markdown;
pub mod prose;
pub mod text;
pub mod tree;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use ubis_core::model::Extracted;
use ubis_core::Store;

/// Files larger than this are not indexed.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Extensions that are never readable text, checked before reading.
const EXCLUDED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "tif", "tiff", "heic", "psd",
    "mp3", "m4a", "aac", "ogg", "wav", "flac", "mp4", "mov", "avi", "mkv", "webm",
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", "jar", "war",
    "bin", "dat", "exe", "dll", "so", "dylib", "o", "a", "lib", "wasm", "class", "pyc",
    "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "hwp", "hwpx", "key", "pages", "numbers",
    "db", "sqlite", "sqlite3", "parquet", "npy", "npz", "pt", "pth", "ckpt", "safetensors", "onnx",
    "ttf", "otf", "woff", "woff2", "lock",
];

/// Directories skipped even without a `.gitignore`.
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", ".git", ".ubis", "__pycache__", ".venv", "venv", "dist", "build"];

/// Admit a file as readable text: allowed extension, bounded size, valid
/// UTF-8, and no NUL bytes. Returns the content when admitted.
pub fn admit(path: &Path, bytes: &[u8]) -> Option<String> {
    let ext = extension(path);
    if EXCLUDED_EXTENSIONS.contains(&ext.as_str()) || bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    (!text.trim().is_empty()).then(|| text.to_string())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Files modified within this window of the last check are re-read even if
/// their stat looks unchanged (same idea as git's racy-clean check).
const RACY_NS: i128 = 2_000_000_000;

fn file_stat(path: &Path) -> Option<(u64, i128)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i128;
    Some((m.len(), mtime))
}

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Run the extractor matching the file's format. `rel` uses `/` separators.
pub fn extract(rel: &str, content: &str) -> Result<Extracted> {
    let ext = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = if rel.contains('.') { ext } else { String::new() };
    if let Some(lang) = code::Lang::from_extension(&ext) {
        return code::extract(rel, content, lang);
    }
    Ok(match ext.as_str() {
        "md" | "markdown" | "mdx" => markdown::extract(rel, content),
        "txt" | "rst" | "org" | "tex" | "" => text::extract(rel, content, true),
        _ => text::extract(rel, content, false),
    })
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexReport {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped: usize,
    pub edges: usize,
}

/// List admissible files under `root` in a deterministic order, respecting
/// `.gitignore`. Returned paths are relative with `/` separators.
pub fn walk(root: &Path) -> Vec<String> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_some_and(|t| t.is_dir()) && SKIPPED_DIRS.contains(&name.as_ref()))
        });
    let mut out = Vec::new();
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
            out.push(rel);
        }
    }
    out.sort();
    out
}

/// Bring the store in line with `root`: add new files, re-extract changed
/// ones (by content hash), drop vanished or no-longer-text files, then
/// re-derive edges if anything changed.
pub fn index_dir(store: &mut Store, root: &Path) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let mut seen = BTreeSet::new();
    let known: BTreeSet<String> = store.file_paths()?.into_iter().collect();
    let stats = store.stat_cache()?;
    let now = now_ns();
    let mut new_stats = Vec::new();
    let mut walked = BTreeSet::new();
    for rel in walk(root) {
        report.scanned += 1;
        let abs = root.join(&rel);
        walked.insert(rel.clone());
        let stat = file_stat(&abs);
        if let (Some((size, mtime)), Some(&(c_size, c_mtime, checked))) = (stat, stats.get(&rel)) {
            // Unchanged stat, and not modified around the last check (a write
            // in the same clock tick could keep size and mtime): skip reading.
            if size == c_size && mtime == c_mtime && mtime + RACY_NS < checked {
                if known.contains(&rel) {
                    seen.insert(rel);
                    report.unchanged += 1;
                } else {
                    report.skipped += 1;
                }
                continue;
            }
        }
        if let Some((size, mtime)) = stat {
            new_stats.push((rel.clone(), size, mtime, now));
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            report.skipped += 1;
            continue;
        };
        let Some(content) = admit(&abs, &bytes) else {
            report.skipped += 1;
            continue;
        };
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        if store.file_hash(&rel)?.as_deref() == Some(hash.as_str()) {
            seen.insert(rel);
            report.unchanged += 1;
            continue;
        }
        match extract(&rel, &content) {
            Ok(ex) => {
                store.replace_file(&ex, &hash, bytes.len() as u64)?;
                if known.contains(&rel) {
                    report.updated += 1;
                } else {
                    report.added += 1;
                }
                seen.insert(rel);
            }
            Err(_) => report.skipped += 1,
        }
    }
    for path in known.difference(&seen) {
        store.remove_file(path)?;
        report.removed += 1;
    }
    let gone: Vec<String> = stats.keys().filter(|p| !walked.contains(*p)).cloned().collect();
    store.update_stat_cache(&new_stats, &gone)?;
    if report.added + report.updated + report.removed > 0 {
        report.edges = store.rebuild_edges()?;
    } else {
        report.edges = store.stats()?.edges;
    }
    Ok(report)
}

/// Re-index only the given relative paths (files or directories), as a file
/// watcher would. Equivalent to [`index_dir`] when `changed` covers every
/// path that actually changed; paths under a changed directory are included.
pub fn index_paths(store: &mut Store, root: &Path, changed: &BTreeSet<String>) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    let allowed: BTreeSet<String> = walk(root).into_iter().collect();
    let known: BTreeSet<String> = store.file_paths()?.into_iter().collect();
    let under = |set: &BTreeSet<String>, c: &str| -> Vec<String> {
        let prefix = format!("{c}/");
        set.range(prefix.clone()..)
            .take_while(|p| p.starts_with(&prefix))
            .cloned()
            .collect()
    };
    let mut targets = BTreeSet::new();
    for c in changed {
        let c = c.trim_end_matches('/');
        targets.insert(c.to_string());
        targets.extend(under(&allowed, c));
        targets.extend(under(&known, c));
    }
    // Force the next full scan to re-read what the watcher saw change.
    let invalid: Vec<String> = targets.iter().cloned().collect();
    store.update_stat_cache(&[], &invalid)?;
    for rel in targets {
        let is_known = known.contains(&rel);
        if !allowed.contains(&rel) {
            if is_known {
                store.remove_file(&rel)?;
                report.removed += 1;
            }
            continue;
        }
        report.scanned += 1;
        let abs = root.join(&rel);
        let content = std::fs::read(&abs).ok().and_then(|b| admit(&abs, &b).map(|c| (c, b.len())));
        let Some((content, size)) = content else {
            if is_known {
                store.remove_file(&rel)?;
                report.removed += 1;
            } else {
                report.skipped += 1;
            }
            continue;
        };
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        if store.file_hash(&rel)?.as_deref() == Some(hash.as_str()) {
            report.unchanged += 1;
            continue;
        }
        match extract(&rel, &content) {
            Ok(ex) => {
                store.replace_file(&ex, &hash, size as u64)?;
                if is_known {
                    report.updated += 1;
                } else {
                    report.added += 1;
                }
            }
            Err(_) => {
                if is_known {
                    store.remove_file(&rel)?;
                    report.removed += 1;
                } else {
                    report.skipped += 1;
                }
            }
        }
    }
    if report.added + report.updated + report.removed > 0 {
        report.edges = store.rebuild_edges()?;
    } else {
        report.edges = store.stats()?.edges;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn admission_rejects_binary() {
        assert!(admit(Path::new("a.txt"), b"hello").is_some());
        assert!(admit(Path::new("a.txt"), b"he\0llo").is_none());
        assert!(admit(Path::new("a.png"), b"hello").is_none());
        assert!(admit(Path::new("a.txt"), &[0xff, 0xfe, 0x41]).is_none());
        assert!(admit(Path::new("a.txt"), b"   \n").is_none());
    }

    /// The core invariant: indexing incrementally through a series of edits
    /// yields exactly the same evidence and edges as indexing from scratch.
    #[test]
    fn incremental_equals_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut inc = Store::open_in_memory().unwrap();

        let steps: Vec<Vec<(&str, Option<&str>)>> = vec![
            vec![
                ("src/a.rs", Some("pub fn helper() {}\npub fn run() { helper(); }\n")),
                ("src/b.rs", Some("pub fn caller() { run(); }\n")),
                ("docs/guide.md", Some("# Guide\nSee `helper` and [b](#usage).\n## Usage\nText.\n")),
            ],
            // Rename a definition: edges into it must disappear, not dangle.
            vec![("src/a.rs", Some("pub fn helper2() {}\npub fn run() { helper2(); }\n"))],
            // New file introduces a second `run` — mass splits.
            vec![("src/c.rs", Some("pub fn run() {}\n"))],
            // Delete a file and turn another into binary.
            vec![("src/c.rs", None), ("docs/guide.md", Some("\0binary"))],
            vec![("docs/guide.md", Some("# Guide\nNow mentions `caller`.\n"))],
        ];
        for step in steps {
            for (rel, body) in step {
                match body {
                    Some(b) => write(root, rel, b),
                    None => std::fs::remove_file(root.join(rel)).unwrap(),
                }
            }
            index_dir(&mut inc, root).unwrap();
            let mut fresh = Store::open_in_memory().unwrap();
            index_dir(&mut fresh, root).unwrap();
            assert_eq!(inc.canonical_dump().unwrap(), fresh.canonical_dump().unwrap());
        }
        // Final state sanity: the doc bridges to `caller`.
        let e = inc.edges_to("src/b.rs::caller").unwrap();
        assert!(e.iter().any(|e| e.src.starts_with("docs/guide.md")));
    }

    /// Watcher path: updating only the touched paths (including a directory
    /// rename) matches a fresh index.
    #[test]
    fn index_paths_equals_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "pub fn helper() {}\n");
        write(root, "src/b.rs", "pub fn run() { helper(); }\n");
        write(root, "docs/x/n.md", "# N\nUses `helper`.\n");
        let mut s = Store::open_in_memory().unwrap();
        index_dir(&mut s, root).unwrap();

        write(root, "src/a.rs", "pub fn helper2() {}\npub fn helper() {}\n");
        std::fs::rename(root.join("docs/x"), root.join("docs/y")).unwrap();
        write(root, "new.txt", "fresh file mentioning run_it\n");
        let changed: BTreeSet<String> =
            ["src/a.rs", "docs/x", "docs/y", "new.txt"].iter().map(|s| s.to_string()).collect();
        let r = index_paths(&mut s, root, &changed).unwrap();
        assert_eq!((r.added, r.updated, r.removed), (2, 1, 1));

        let mut fresh = Store::open_in_memory().unwrap();
        index_dir(&mut fresh, root).unwrap();
        assert_eq!(s.canonical_dump().unwrap(), fresh.canonical_dump().unwrap());
    }

    /// A scope in the title (`printer:`) names a path; among units that match
    /// the text equally, the one in that path ranks first.
    #[test]
    fn path_terms_rank_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/printer/json.rs", "pub fn finish() { let stats = 1; }\n");
        write(root, "src/search/core.rs", "pub fn finish() { let stats = 2; }\n");
        let mut s = Store::open_in_memory().unwrap();
        index_dir(&mut s, root).unwrap();
        let hits = ubis_core::search(&s, &ubis_core::Query::text("printer: fix stats in finish"))
            .unwrap()
            .hits;
        assert_eq!(hits[0].id, "src/printer/json.rs::finish");
        assert!(hits[0].via.iter().any(|v| v.op == "path"));
    }

    /// An index written by another schema/tokenizer version is reset on
    /// open and refilled by the next index, keeping only the root.
    #[test]
    fn old_index_version_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", "# A\ntext\n");
        let db = dir.path().join("x.db");
        {
            let mut s = Store::open(&db).unwrap();
            index_dir(&mut s, dir.path()).unwrap();
            s.set_meta("root", "/r").unwrap();
            s.conn().execute("UPDATE meta SET value='0' WHERE key='schema'", []).unwrap();
        }
        let mut s = Store::open(&db).unwrap();
        assert_eq!(s.stats().unwrap().files, 0);
        assert_eq!(s.meta("root").unwrap().as_deref(), Some("/r"));
        let r = index_dir(&mut s, dir.path()).unwrap();
        assert_eq!(r.added, 1);
    }

    #[test]
    fn unchanged_files_are_not_reextracted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", "# A\ntext\n");
        let mut s = Store::open_in_memory().unwrap();
        let r1 = index_dir(&mut s, dir.path()).unwrap();
        let r2 = index_dir(&mut s, dir.path()).unwrap();
        assert_eq!((r1.added, r2.added, r2.unchanged), (1, 0, 1));
    }
}
