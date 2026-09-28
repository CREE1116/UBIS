//! Git history → the indexed units each commit touched.
//!
//! A hunk's line range refers to the file *as of that commit*, so the file is
//! re-extracted at that revision and its leaves are mapped onto units of the
//! current index:
//!
//! * named units (`path::Type::method`, `path#section`) match by ID;
//! * ordinal units (`¶3`, `~2`, `code1`) shift when text is inserted above
//!   them, so they match the current ordinal leaf in the same file with the
//!   highest token Jaccard (≥ [`ORDINAL_JACCARD`]).
//!
//! Blobs are read through one `git cat-file --batch` process and each
//! `(path, blob)` is extracted at most once; paths that are not indexed now
//! are never read.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use ubis_core::cochange::{CoChangeIndex, CoChangeParams};
use ubis_core::model::UnitId;
use ubis_core::tokenize::tokenize;
use ubis_core::Store;
use ubis_git::{BlobReader, Commit};

pub const ORDINAL_JACCARD: f64 = 0.3;

struct Leaf {
    start: usize,
    end: usize,
    id: UnitId,
    ordinal: bool,
    text: String,
    /// Lazily computed mapping onto a current unit.
    mapped: Option<Option<UnitId>>,
}

pub struct UnitMapper<'a> {
    store: &'a Store,
    reader: BlobReader,
    indexed: HashSet<String>,
    blobs: HashMap<(String, String), Vec<Leaf>>,
    /// Current ordinal leaves per path with their token sets.
    current: HashMap<String, Vec<(UnitId, BTreeSet<String>)>>,
}

impl<'a> UnitMapper<'a> {
    pub fn new(store: &'a Store, repo: &Path) -> Result<Self> {
        Ok(Self {
            store,
            reader: BlobReader::new(repo)?,
            indexed: store.file_paths()?.into_iter().collect(),
            blobs: HashMap::new(),
            current: HashMap::new(),
        })
    }

    /// Leaf units of the current index touched by `c`.
    pub fn touched(&mut self, c: &Commit) -> Result<BTreeSet<UnitId>> {
        let mut out = BTreeSet::new();
        for path in c.files() {
            if !self.indexed.contains(path) {
                continue;
            }
            let Some(blob) = c.blob(path) else { continue };
            let key = (path.to_string(), blob.to_string());
            if !self.blobs.contains_key(&key) {
                let leaves = self.load(path, blob)?;
                self.blobs.insert(key.clone(), leaves);
            }
            let mut leaves = self.blobs.remove(&key).unwrap_or_default();
            for h in c.hunks.iter().filter(|h| h.path == path) {
                let (a, b) = (h.start, h.start + h.len - 1);
                for leaf in leaves.iter_mut() {
                    if leaf.start <= b && a <= leaf.end {
                        if leaf.mapped.is_none() {
                            leaf.mapped = Some(self.map(path, leaf)?);
                        }
                        if let Some(Some(id)) = &leaf.mapped {
                            out.insert(id.clone());
                        }
                    }
                }
            }
            self.blobs.insert(key, leaves);
        }
        Ok(out)
    }

    fn load(&mut self, path: &str, blob: &str) -> Result<Vec<Leaf>> {
        let Some(content) = self.reader.read(blob)? else { return Ok(Vec::new()) };
        let Some(content) = crate::admit(Path::new(path), content.as_bytes()) else {
            return Ok(Vec::new());
        };
        let Ok(ex) = crate::extract(path, &content) else { return Ok(Vec::new()) };
        let parents: BTreeSet<&str> = ex.units.iter().filter_map(|u| u.parent.as_deref()).collect();
        Ok(ex
            .units
            .iter()
            .filter(|u| !parents.contains(u.id.as_str()))
            .map(|u| Leaf {
                start: u.start_line,
                end: u.end_line,
                ordinal: is_ordinal(&u.id),
                id: u.id.clone(),
                text: if is_ordinal(&u.id) { u.text.clone() } else { String::new() },
                mapped: None,
            })
            .collect())
    }

    fn map(&mut self, path: &str, leaf: &Leaf) -> Result<Option<UnitId>> {
        if !leaf.ordinal {
            return Ok(self.store.unit(&leaf.id)?.map(|u| u.id));
        }
        if !self.current.contains_key(path) {
            let cur = self
                .store
                .units_in_file(path)?
                .into_iter()
                .filter(|u| u.is_leaf && is_ordinal(&u.id))
                .map(|u| (u.id, tokenize(&u.text).into_iter().collect()))
                .collect();
            self.current.insert(path.to_string(), cur);
        }
        let toks: BTreeSet<String> = tokenize(&leaf.text).into_iter().collect();
        Ok(self.current[path]
            .iter()
            .map(|(id, t)| (jaccard(&toks, t), id))
            .filter(|(j, _)| *j >= ORDINAL_JACCARD)
            .max_by(|x, y| x.0.total_cmp(&y.0).then_with(|| y.1.cmp(x.1)))
            .map(|(_, id)| id.clone()))
    }
}

/// Bump when the co-change derivation or its parameters change.
const COCHANGE_VERSION: &str = "cochange-v1";
pub const BASIS_KEY: &str = "git_basis";

/// Everything the derived git tables depend on: HEAD, the indexed files, and
/// the derivation version. Equal basis ⇒ rebuilding would give the same rows.
pub fn git_basis(store: &Store, repo: &Path) -> Result<String> {
    let head = ubis_git::rev_parse(repo, "HEAD")?;
    let mut h = blake3::Hasher::new();
    for (path, hash) in store.file_hashes()? {
        h.update(path.as_bytes());
        h.update(b"\0");
        h.update(hash.as_bytes());
        h.update(b"\n");
    }
    Ok(format!("{COCHANGE_VERSION}:{head}:{}", h.finalize().to_hex()))
}

/// `(commit time, touched units)` for every commit touching ≥ 2 indexed units.
pub fn transactions(store: &Store, repo: &Path, commits: &[Commit]) -> Result<Vec<(i64, BTreeSet<UnitId>)>> {
    let mut mapper = UnitMapper::new(store, repo)?;
    let mut tx = Vec::new();
    for c in commits {
        let s = mapper.touched(c)?;
        if s.len() >= 2 {
            tx.push((c.ts, s));
        }
    }
    Ok(tx)
}

/// Recompute the derived co-change table from `commits` against the current
/// index. Returns the number of unit pairs stored.
pub fn rebuild_cochange(store: &mut Store, repo: &Path, commits: &[Commit], p: &CoChangeParams) -> Result<usize> {
    let tx = transactions(store, repo, commits)?;
    store.replace_cochange(&CoChangeIndex::build(&tx, p))
}

pub fn is_ordinal(id: &str) -> bool {
    let last = id.rsplit(['/', '#']).next().unwrap_or("");
    last.starts_with('¶') || last.starts_with('~') || last.starts_with("code") || !id.contains(['/', '#', ':'])
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len() as f64 + b.len() as f64 - inter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn commit(root: &Path, files: &[(&str, &str)], msg: &str) {
        for (rel, body) in files {
            std::fs::write(root.join(rel), body).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", msg]);
    }

    const A1: &str = "pub fn x() {\n    1;\n}\n\npub fn y() {\n    1;\n}\n\npub fn z() {\n    1;\n}\n";
    const A2: &str = "pub fn x() {\n    2;\n}\n\npub fn y() {\n    2;\n}\n\npub fn z() {\n    1;\n}\n";
    const A3: &str = "pub fn x() {\n    3;\n}\n\npub fn y() {\n    3;\n}\n\npub fn z() {\n    1;\n}\n";
    const N1: &str = "alpha beta gamma delta.\n\nepsilon zeta eta theta iota.\n";
    // A paragraph inserted above shifts the edited one from ¶2 to ¶3.
    const N2: &str = "new intro paragraph here.\n\nalpha beta gamma delta.\n\nepsilon zeta eta theta iota kappa.\n";

    #[test]
    fn cochange_from_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        commit(root, &[("a.rs", A1), ("notes.txt", N1)], "init");
        commit(root, &[("a.rs", A2)], "x and y");
        commit(root, &[("a.rs", A3)], "x and y again");
        commit(root, &[("notes.txt", N2)], "notes");

        let mut store = Store::open_in_memory().unwrap();
        crate::index_dir(&mut store, root).unwrap();
        let history = ubis_git::history(root, "HEAD", 100).unwrap();
        let now = history.last().unwrap().ts;

        // Ordinal leaves map onto the current unit despite the shift.
        let mut m = UnitMapper::new(&store, root).unwrap();
        let notes = m.touched(&history[3]).unwrap();
        let last_para = store
            .units_in_file("notes.txt")
            .unwrap()
            .into_iter()
            .filter(|u| u.is_leaf && u.text.contains("epsilon"))
            .map(|u| u.id)
            .next()
            .unwrap();
        assert!(notes.contains(&last_para), "{notes:?} lacks {last_para}");
        let xy: BTreeSet<UnitId> = ["a.rs::x", "a.rs::y"].iter().map(|s| s.to_string()).collect();
        assert_eq!(m.touched(&history[1]).unwrap(), xy);
        drop(m);

        let pairs = rebuild_cochange(&mut store, root, &history, &CoChangeParams::at(now)).unwrap();
        assert_eq!(pairs, 1); // x–y twice; everything else below support
        let n = store.cochange_from("a.rs::x").unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].0, "a.rs::y");

        // Derived table is a pure function of evidence.
        let mut again = Store::open_in_memory().unwrap();
        crate::index_dir(&mut again, root).unwrap();
        rebuild_cochange(&mut again, root, &history, &CoChangeParams::at(now)).unwrap();
        assert_eq!(store.canonical_dump().unwrap(), again.canonical_dump().unwrap());
    }
}
