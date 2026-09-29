//! Git history → the indexed units each commit touched.
//!
//! A hunk's line range refers to the file *as of that commit*, so the file is
//! re-extracted at that revision and its leaves are mapped onto units of the
//! current index:
//!
//! * named units (`path::Type::method`, `path#section`) match by ID;
//! * ordinal units ([`UnitKind::is_ordinal`]: `¶3`, `~2`, `code1`) shift
//!   when text is inserted above them, so they match the current ordinal
//!   leaf in the same file with the highest token Jaccard
//!   (≥ [`ORDINAL_JACCARD`]).
//!
//! [`History`] reads blobs through one `git cat-file --batch` process and
//! extracts each `(path, blob)` at most once, across any number of indexes;
//! paths that are not indexed now are never read.
//!
//! [`UnitKind::is_ordinal`]: ubis_core::model::UnitKind::is_ordinal

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use ubis_core::cochange::{CoChangeIndex, CoChangeParams};
use ubis_core::model::UnitId;
use ubis_core::tokenize::tokenize_raw as tokenize;
use ubis_core::store::CommitRow;
use ubis_core::Store;
use ubis_git::{BlobReader, Commit};

pub const ORDINAL_JACCARD: f64 = 0.3;

/// A leaf of a file as of some commit.
struct Leaf {
    start: usize,
    end: usize,
    id: UnitId,
    /// Token set, kept only for ordinal leaves (the only ones matched by content).
    tokens: Option<BTreeSet<String>>,
}

/// Git access plus a parse cache keyed by `(path, blob)`. Independent of any
/// index, so it can be reused across many stores (e.g. one per evaluation task).
pub struct History {
    reader: BlobReader,
    leaves: HashMap<(String, String), Rc<Vec<Leaf>>>,
}

impl History {
    pub fn open(repo: &Path) -> Result<Self> {
        Ok(Self {
            reader: BlobReader::new(repo)?,
            leaves: HashMap::new(),
        })
    }

    /// A mapper onto the units of `store`. Drop it before changing the store.
    pub fn mapper<'a>(&'a mut self, store: &'a Store) -> Result<UnitMapper<'a>> {
        Ok(UnitMapper {
            indexed: store.file_paths()?.into_iter().collect(),
            store,
            history: self,
            mapped: HashMap::new(),
            current: HashMap::new(),
        })
    }

    /// Record `commits` as evidence, then derive from what was recorded (the
    /// store, not git, is the source). Returns co-change unit pairs.
    pub fn record_and_derive(&mut self, store: &mut Store, commits: &[Commit], p: &CoChangeParams) -> Result<usize> {
        let rows: Vec<CommitRow> = commits.iter().map(to_row).collect();
        store.replace_history(&rows)?;
        let commits: Vec<Commit> = store.history()?.into_iter().map(from_row).collect();
        self.derive(store, &commits, p)
    }

    /// Map every commit onto the current units once, then derive both views
    /// of the same transactions `(time, message, units)`:
    ///
    /// * co-change — which units changed together;
    /// * the history field — per unit, the words people used when changing it
    ///   (searched like the code itself, see `query::Field::History`).
    ///
    /// Bulk commits (more than `max_set` units) feed neither: their message
    /// says nothing specific about any one unit.
    pub fn derive(&mut self, store: &mut Store, commits: &[Commit], p: &CoChangeParams) -> Result<usize> {
        let mut tx = Vec::new();
        let mut field: BTreeMap<UnitId, (BTreeMap<String, f64>, f64)> = BTreeMap::new();
        let mut changes: Vec<(UnitId, String)> = Vec::new();
        {
            let mut mapper = self.mapper(store)?;
            for c in commits {
                let units = mapper.touched(c)?;
                if units.is_empty() || units.len() > p.max_set {
                    continue;
                }
                changes.extend(units.iter().map(|u| (u.clone(), c.id.clone())));
                let words = ubis_core::tokenize::tokenize(&format!("{}\n{}", c.subject, c.body));
                for u in &units {
                    let doc = field.entry(u.clone()).or_default();
                    for w in &words {
                        *doc.0.entry(w.clone()).or_default() += 1.0;
                    }
                    doc.1 += words.len() as f64;
                }
                if units.len() >= 2 {
                    tx.push((c.ts, units));
                }
            }
        }
        store.replace_history_field(&field)?;
        store.replace_unit_changes(&changes)?;
        store.replace_cochange(&CoChangeIndex::build(&tx, p))
    }

    fn leaves(&mut self, path: &str, blob: &str) -> Result<Rc<Vec<Leaf>>> {
        let key = (path.to_string(), blob.to_string());
        if let Some(l) = self.leaves.get(&key) {
            return Ok(l.clone());
        }
        let parsed = self
            .reader
            .read(blob)?
            .and_then(|c| crate::admit(Path::new(path), c.as_bytes()))
            .and_then(|c| crate::extract(path, &c).ok());
        let leaves = match parsed {
            Some(ex) => {
                let parents: BTreeSet<&str> = ex.units.iter().filter_map(|u| u.parent.as_deref()).collect();
                ex.units
                    .iter()
                    .filter(|u| !parents.contains(u.id.as_str()))
                    .map(|u| Leaf {
                        start: u.start_line,
                        end: u.end_line,
                        id: u.id.clone(),
                        tokens: u.kind.is_ordinal().then(|| tokenize(&u.text).into_iter().collect()),
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let leaves = Rc::new(leaves);
        self.leaves.insert(key, leaves.clone());
        Ok(leaves)
    }
}

/// Maps commit hunks onto the units of one index state.
pub struct UnitMapper<'a> {
    store: &'a Store,
    history: &'a mut History,
    indexed: HashSet<String>,
    /// `(path, historical leaf id, its content)` → current unit.
    mapped: HashMap<(String, UnitId, u64), Option<UnitId>>,
    /// Current ordinal leaves per path with their token sets.
    current: HashMap<String, Vec<(UnitId, BTreeSet<String>)>>,
}

impl UnitMapper<'_> {
    /// Leaf units of the current index touched by `c`.
    pub fn touched(&mut self, c: &Commit) -> Result<BTreeSet<UnitId>> {
        let mut out = BTreeSet::new();
        for path in c.files() {
            if !self.indexed.contains(path) {
                continue; // not indexed now: nothing to map onto, never read
            }
            let Some(blob) = c.blob(path) else { continue };
            let leaves = self.history.leaves(path, blob)?;
            for h in c.hunks.iter().filter(|h| h.path == path) {
                let (a, b) = (h.start, h.start + h.len - 1);
                for leaf in leaves.iter().filter(|l| l.start <= b && a <= l.end) {
                    if let Some(id) = self.map(path, leaf)? {
                        out.insert(id);
                    }
                }
            }
        }
        Ok(out)
    }

    fn map(&mut self, path: &str, leaf: &Leaf) -> Result<Option<UnitId>> {
        let Some(tokens) = &leaf.tokens else {
            return Ok(self.store.unit(&leaf.id)?.map(|u| u.id));
        };
        let key = (path.to_string(), leaf.id.clone(), token_key(tokens));
        if let Some(m) = self.mapped.get(&key) {
            return Ok(m.clone());
        }
        if !self.current.contains_key(path) {
            let cur = self
                .store
                .units_in_file(path)?
                .into_iter()
                .filter(|u| u.is_leaf && u.kind.is_ordinal())
                .map(|u| (u.id, tokenize(&u.text).into_iter().collect()))
                .collect();
            self.current.insert(path.to_string(), cur);
        }
        let best = self.current[path]
            .iter()
            .map(|(id, t)| (jaccard(tokens, t), id))
            .filter(|(j, _)| *j >= ORDINAL_JACCARD)
            .max_by(|x, y| x.0.total_cmp(&y.0).then_with(|| y.1.cmp(x.1)))
            .map(|(_, id)| id.clone());
        self.mapped.insert(key, best.clone());
        Ok(best)
    }
}

/// Order-independent fingerprint of a token set (cache key only).
fn token_key(tokens: &BTreeSet<String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tokens.hash(&mut h);
    h.finish()
}

/// Bump when the co-change derivation or its parameters change.
const COCHANGE_VERSION: &str = "derive-v3";
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

pub fn to_row(c: &Commit) -> CommitRow {
    CommitRow {
        id: c.id.clone(),
        ts: c.ts,
        subject: c.subject.clone(),
        body: c.body.clone(),
        merge: c.merge,
        hunks: c.hunks.iter().map(|h| (h.path.clone(), h.start, h.len)).collect(),
        blobs: c.blobs.clone(),
    }
}

pub fn from_row(r: CommitRow) -> Commit {
    Commit {
        id: r.id,
        ts: r.ts,
        subject: r.subject,
        body: r.body,
        merge: r.merge,
        hunks: r
            .hunks
            .into_iter()
            .map(|(path, start, len)| ubis_git::Hunk { path, start, len })
            .collect(),
        blobs: r.blobs,
    }
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
        let mut h = History::open(root).unwrap();
        let mut m = h.mapper(&store).unwrap();
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
        drop(h);

        let pairs = History::open(root)
            .unwrap()
            .record_and_derive(&mut store, &history, &CoChangeParams::at(now))
            .unwrap();
        assert_eq!(pairs, 1); // x–y twice; everything else below support
        // The history field: a commit's words belong to the units it touched.
        let with_again: Vec<String> =
            store.hist_postings("again").unwrap().into_iter().map(|p| p.0).collect();
        assert_eq!(with_again, vec!["a.rs::x".to_string(), "a.rs::y".to_string()]);
        assert!(store.hist_postings("notes").unwrap().iter().all(|p| p.0.starts_with("notes.txt")));
        let n = store.cochange_from("a.rs::x").unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].0, "a.rs::y");

        // Derived table is a pure function of evidence.
        let mut again = Store::open_in_memory().unwrap();
        crate::index_dir(&mut again, root).unwrap();
        History::open(root)
            .unwrap()
            .record_and_derive(&mut again, &history, &CoChangeParams::at(now))
            .unwrap();
        assert_eq!(
            again.history().unwrap(),
            history.iter().map(to_row).collect::<Vec<_>>(),
            "recorded history round-trips"
        );
        assert_eq!(store.canonical_dump().unwrap(), again.canonical_dump().unwrap());
    }
}
