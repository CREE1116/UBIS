//! One open index plus the operations the CLI exposes.
//!
//! A session keeps itself current: before answering, [`Session::refresh`]
//! re-indexes changed files (cheap: unchanged files are skipped by stat) and
//! re-derives git evidence when `HEAD` moved.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ubis_core::query::{search, Hit, Query};
use ubis_core::{Edge, Store, UnitRow};
use ubis_ingest::IndexReport;

pub const DB_DIR: &str = ".ubis";
pub const DB_FILE: &str = "index.db";
const ROOT_KEY: &str = "root";
const HEAD_KEY: &str = "git_head";

pub struct Session {
    pub store: Store,
    pub root: PathBuf,
}

impl Session {
    /// Open the index for `db`, or the nearest `.ubis/index.db` above the
    /// current directory. Inside a git repository with no index yet, create
    /// one at the repository root.
    pub fn locate(db: Option<&Path>) -> Result<Self> {
        if let Some(db) = db {
            return Self::at(db.to_path_buf());
        }
        let cwd = std::env::current_dir()?;
        if let Some(db) = find_db(&cwd) {
            return Self::at(db);
        }
        let Ok(top) = ubis_git::toplevel(&cwd) else {
            bail!("no .ubis/index.db here or above, and not in a git repository; run `ubis index <dir>`");
        };
        eprintln!("ubis: first run, indexing {}", top.display());
        let mut s = Self::create(&top, &top.join(DB_DIR).join(DB_FILE))?;
        let r = s.index(true, true)?;
        eprintln!("ubis: {}", r);
        Ok(s)
    }

    fn at(db: PathBuf) -> Result<Self> {
        let store = Store::open(&db)?;
        let root = match store.meta(ROOT_KEY)? {
            Some(r) => PathBuf::from(r),
            // Indexes made before the root was recorded: `<root>/.ubis/index.db`.
            None => db
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .context("cannot infer the indexed directory; run `ubis index <dir>`")?,
        };
        Ok(Self { store, root })
    }

    pub fn create(root: &Path, db: &Path) -> Result<Self> {
        let mut store = Store::open(db)?;
        // Keep the index out of the user's `git status`.
        if let Some(dir) = db.parent().filter(|d| d.ends_with(DB_DIR)) {
            let ignore = dir.join(".gitignore");
            if !ignore.exists() {
                std::fs::write(ignore, "*\n").ok();
            }
        }
        store.set_meta(ROOT_KEY, &root.to_string_lossy())?;
        Ok(Self {
            store,
            root: root.to_path_buf(),
        })
    }

    /// Bring the index up to date. `exact` re-derives git evidence whenever
    /// the indexed files changed (so the result equals a fresh index);
    /// otherwise only when `HEAD` moved.
    pub fn index(&mut self, git: bool, exact: bool) -> Result<Refresh> {
        let started = std::time::Instant::now();
        let files = ubis_ingest::index_dir(&mut self.store, &self.root)?;
        let git = if git && ubis_git::is_repo(&self.root) {
            self.index_git(exact)?
        } else {
            None
        };
        Ok(Refresh {
            files,
            git,
            secs: started.elapsed().as_secs_f64(),
        })
    }

    /// Quiet refresh before answering a query.
    pub fn refresh(&mut self) -> Result<Refresh> {
        if !self.root.is_dir() {
            bail!("indexed directory {} no longer exists", self.root.display());
        }
        self.index(true, false)
    }

    fn index_git(&mut self, exact: bool) -> Result<Option<GitRefresh>> {
        use ubis_ingest::history::{git_basis, History, BASIS_KEY};
        let Ok(head) = ubis_git::rev_parse(&self.root, "HEAD") else {
            return Ok(None); // no commits yet
        };
        if !exact && self.store.meta(HEAD_KEY)?.as_deref() == Some(head.as_str()) {
            return Ok(None);
        }
        let basis = git_basis(&self.store, &self.root)?;
        if self.store.meta(BASIS_KEY)?.as_deref() != Some(basis.as_str()) {
            let history = ubis_git::history(&self.root, "HEAD", 5000)?;
            let now = history.last().map(|c| c.ts).unwrap_or(0);
            let params = ubis_core::cochange::CoChangeParams::at(now);
            let pairs = History::open(&self.root)?.record_and_derive(&mut self.store, &history, &params)?;
            self.store.set_meta(BASIS_KEY, &basis)?;
            self.store.set_meta(HEAD_KEY, &head)?;
            return Ok(Some(GitRefresh {
                commits: history.len(),
                pairs,
            }));
        }
        self.store.set_meta(HEAD_KEY, &head)?;
        Ok(None)
    }

    /// A unit by ID, `Owner::name` suffix, label, or `path:line` (the
    /// smallest unit containing that line; path relative to the indexed
    /// root, the current directory, or absolute). Ambiguous names fail with
    /// the candidate list instead of silently picking one.
    pub fn resolve(&self, reference: &str) -> Result<UnitRow> {
        if let Some((path, line)) = split_path_line(reference) {
            for rel in self.relative_paths(path) {
                if let Some(u) = self.store.unit_at(&rel, line)? {
                    return Ok(u);
                }
            }
            bail!("no indexed unit at `{reference}`");
        }
        let matches = self.store.find_unit(reference)?;
        match matches.len() {
            0 => bail!("no unit matches `{reference}`"),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => {
                if let Some(u) = matches.iter().find(|u| u.id == reference) {
                    return Ok(u.clone());
                }
                let list: Vec<_> = matches.iter().take(10).map(|u| u.id.as_str()).collect();
                bail!("`{reference}` is ambiguous:\n  {}", list.join("\n  "))
            }
        }
    }

    /// Candidate root-relative spellings of a user-supplied path.
    fn relative_paths(&self, path: &str) -> Vec<String> {
        let mut out = Vec::new();
        let p = Path::new(path);
        let mut push = |p: &Path| {
            if let Ok(rel) = p.strip_prefix(&self.root) {
                out.push(to_slash(rel));
            }
        };
        if p.is_absolute() {
            push(p);
        } else if let Ok(cwd) = std::env::current_dir() {
            push(&cwd.join(p));
            if let Ok(c) = cwd.join(p).canonicalize() {
                push(&c);
            }
        }
        out.push(path.trim_start_matches("./").to_string());
        out.dedup();
        out
    }

    pub fn find(&self, text: &str, anchor: Option<&str>, scope: Option<&str>, k: usize) -> Result<Vec<Hit>> {
        let anchor = match anchor {
            Some(a) => Some(self.resolve(a)?.id),
            None => None,
        };
        if text.trim().is_empty() && anchor.is_none() {
            bail!("give query text, an anchor, or both");
        }
        let q = Query {
            text: text.to_string(),
            anchor,
            scope: scope.map(str::to_string),
            k_max: k,
            k_min: None,
        };
        Ok(search(&self.store, &q)?.hits)
    }

    pub fn near(&self, unit: &str, k: usize) -> Result<Vec<Hit>> {
        self.find("", Some(unit), None, k)
    }

    pub fn open(&self, unit: &str, out: bool) -> Result<(UnitRow, Vec<UnitRow>)> {
        let mut u = self.resolve(unit)?;
        if out {
            if let Some(p) = u.parent.clone() {
                u = self.store.unit(&p)?.context("parent missing")?;
            }
        }
        let children = self.store.children(&u.id)?;
        Ok((u, children))
    }

    pub fn refs(&self, unit: &str) -> Result<(UnitRow, Vec<Edge>)> {
        let u = self.resolve(unit)?;
        let mut edges = Vec::new();
        for s in self.store.subtree(&u.id)? {
            edges.extend(self.store.edges_to(&s.id)?);
        }
        edges.sort_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.src.cmp(&b.src)));
        Ok((u, edges))
    }

    // ------------------------------------------------------------ rendering

    pub fn render_hits(hits: &[Hit]) -> String {
        if hits.is_empty() {
            return "no candidates\n".into();
        }
        let mut s = String::new();
        for (i, h) in hits.iter().enumerate() {
            let lifted = if h.lifted > 0 {
                format!("  [{} matches inside]", h.lifted)
            } else {
                String::new()
            };
            s += &format!(
                "[{}] {}:{}-{}  {}  ({}){}\n",
                i + 1,
                h.path,
                h.start_line,
                h.end_line,
                h.id,
                h.kind.as_str(),
                lifted
            );
            if !h.signature.is_empty() {
                s += &format!("    {}\n", h.signature);
            }
            let via: Vec<_> = h.via.iter().map(|v| format!("{} {:.2}", v.op, v.contribution)).collect();
            s += &format!("    via {}\n", via.join(", "));
        }
        s
    }

    pub fn render_open(u: &UnitRow, children: &[UnitRow]) -> String {
        let mut s = format!("{}:{}-{}  {}  ({})\n", u.path, u.start_line, u.end_line, u.id, u.kind.as_str());
        if let Some(p) = &u.parent {
            s += &format!("parent: {p}\n");
        }
        if children.is_empty() {
            s += "---\n";
            for (i, line) in u.text.lines().enumerate() {
                s += &format!("{:>5}  {}\n", u.start_line + i, line);
            }
        } else {
            s += "children:\n";
            for c in children {
                s += &format!("  {:>5}-{:<5} {}  ({})\n", c.start_line, c.end_line, c.id, c.kind.as_str());
            }
        }
        s
    }

    pub fn render_refs(&self, u: &UnitRow, edges: &[Edge]) -> Result<String> {
        if edges.is_empty() {
            return Ok(format!("no recorded references to {}\n", u.id));
        }
        let mut s = String::new();
        for e in edges {
            let loc = self
                .store
                .unit(&e.src)?
                .map(|src| format!("{}:{}", src.path, e.line))
                .unwrap_or_default();
            s += &format!(
                "{:<6} {:<9} w={:.2}  {}  ({})  → {}\n",
                e.kind.as_str(),
                e.origin,
                e.weight,
                e.src,
                loc,
                e.dst
            );
        }
        Ok(s)
    }
}

pub struct GitRefresh {
    pub commits: usize,
    pub pairs: usize,
}

pub struct Refresh {
    pub files: IndexReport,
    pub git: Option<GitRefresh>,
    pub secs: f64,
}

impl Refresh {
    pub fn changed(&self) -> bool {
        let f = &self.files;
        f.added + f.updated + f.removed > 0 || self.git.is_some()
    }
}

impl std::fmt::Display for Refresh {
    fn fmt(&self, fm: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let f = &self.files;
        write!(
            fm,
            "{:.2}s — added {}, updated {}, unchanged {}, removed {}, skipped {}; edges {}",
            self.secs, f.added, f.updated, f.unchanged, f.removed, f.skipped, f.edges
        )?;
        if let Some(g) = &self.git {
            write!(fm, "; commits {}, co-change pairs {}", g.commits, g.pairs)?;
        }
        Ok(())
    }
}

pub fn find_db(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let p = dir.join(DB_DIR).join(DB_FILE);
        if p.exists() {
            return Some(p);
        }
        cur = dir.parent();
    }
    None
}

/// `src/a.rs:120` → `("src/a.rs", 120)`. `Type::method` is not a match.
fn split_path_line(s: &str) -> Option<(&str, usize)> {
    let (path, line) = s.rsplit_once(':')?;
    if path.is_empty() || path.ends_with(':') || line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((path, line.parse().ok()?))
}

fn to_slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_line_syntax() {
        assert_eq!(split_path_line("src/a.rs:120"), Some(("src/a.rs", 120)));
        assert_eq!(split_path_line("Store::open"), None);
        assert_eq!(split_path_line("a.rs::x"), None);
        assert_eq!(split_path_line("a.rs:"), None);
        assert_eq!(split_path_line("C:\\x\\a.rs:3"), Some(("C:\\x\\a.rs", 3)));
    }
}
