//! UBIS ingest: text admission, format extractors, and the incremental indexer.

pub mod code;
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
    for rel in walk(root) {
        report.scanned += 1;
        let abs = root.join(&rel);
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
