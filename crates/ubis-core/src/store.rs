//! Evidence store backed by SQLite.
//!
//! The store holds *evidence*: units, postings, definitions, mentions, and git
//! hunks. Edges are derived from mentions ⋈ definitions and can be rebuilt at
//! any time. Everything a query reads comes from here; there is no separate
//! snapshot file.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::*;
use crate::resolve;
use crate::tokenize::tokenize;

pub const SCHEMA_VERSION: i64 = 1;

/// `(commit id, unix time, subject, [(path, start line, line count)])`.
pub type CommitRow = (String, i64, String, Vec<(String, usize, usize)>);

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
    path TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS units(
    id TEXT PRIMARY KEY,
    parent TEXT,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    label TEXT NOT NULL,
    text TEXT NOT NULL,
    is_leaf INTEGER NOT NULL,
    len REAL NOT NULL,
    ord INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS units_path ON units(path);
CREATE INDEX IF NOT EXISTS units_parent ON units(parent);
CREATE TABLE IF NOT EXISTS postings(term TEXT NOT NULL, unit_id TEXT NOT NULL, tf REAL NOT NULL, path TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS postings_term ON postings(term);
CREATE INDEX IF NOT EXISTS postings_path ON postings(path);
CREATE TABLE IF NOT EXISTS definitions(unit_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS definitions_name ON definitions(name);
CREATE INDEX IF NOT EXISTS definitions_name_nocase ON definitions(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS definitions_path ON definitions(path);
CREATE TABLE IF NOT EXISTS mentions(unit_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, path TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS mentions_name ON mentions(name);
CREATE INDEX IF NOT EXISTS mentions_path ON mentions(path);
CREATE TABLE IF NOT EXISTS edges(
    src TEXT NOT NULL, dst TEXT NOT NULL, kind TEXT NOT NULL,
    origin TEXT NOT NULL, weight REAL NOT NULL, line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE TABLE IF NOT EXISTS commits(id TEXT PRIMARY KEY, ts INTEGER NOT NULL, subject TEXT NOT NULL, seq INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS hunks(commit_id TEXT NOT NULL, path TEXT NOT NULL, start_line INTEGER NOT NULL, len INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS hunks_commit ON hunks(commit_id);
"#;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UnitRow {
    pub id: UnitId,
    pub parent: Option<UnitId>,
    pub kind: UnitKind,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub label: String,
    pub text: String,
    pub is_leaf: bool,
    pub len: f64,
    pub ord: i64,
}

impl UnitRow {
    pub fn signature(&self) -> String {
        Unit {
            id: self.id.clone(),
            parent: None,
            kind: self.kind,
            path: String::new(),
            start_line: 0,
            end_line: 0,
            label: String::new(),
            text: self.text.clone(),
        }
        .signature()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct Stats {
    pub files: usize,
    pub units: usize,
    pub leaves: usize,
    pub avg_len: f64,
    pub definitions: usize,
    pub mentions: usize,
    pub edges: usize,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0))
            .optional()?;
        match version {
            None => {
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES('schema', ?1)",
                    [SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => anyhow::ensure!(
                v == SCHEMA_VERSION.to_string(),
                "index schema {v} is not supported (expected {SCHEMA_VERSION}); delete the index and rebuild"
            ),
        }
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ----------------------------------------------------------------- files

    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT hash FROM files WHERE path=?1", [path], |r| r.get(0))
            .optional()?)
    }

    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replace everything known about one file in a single transaction.
    pub fn replace_file(&mut self, ex: &Extracted, hash: &str, size: u64) -> Result<()> {
        let tx = self.conn.transaction()?;
        delete_file_rows(&tx, &ex.path)?;
        tx.execute(
            "INSERT INTO files(path, hash, size) VALUES(?1, ?2, ?3)",
            params![ex.path, hash, size as i64],
        )?;
        insert_extracted(&tx, ex)?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_file(&mut self, path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        delete_file_rows(&tx, path)?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- edges

    /// Recompute all edges from mentions ⋈ definitions.
    pub fn rebuild_edges(&mut self) -> Result<usize> {
        let defs = self.all_definitions()?;
        let mentions = self.all_mentions()?;
        let edges = resolve::resolve(&defs, &mentions);
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM edges", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO edges(src, dst, kind, origin, weight, line) VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            for e in &edges {
                stmt.execute(params![
                    e.src,
                    e.dst,
                    e.kind.as_str(),
                    e.origin,
                    e.weight,
                    e.line as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(edges.len())
    }

    fn all_definitions(&self) -> Result<Vec<(Definition, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT unit_id, name, kind, path FROM definitions ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| {
            let kind: String = r.get(2)?;
            Ok((
                Definition {
                    unit_id: r.get(0)?,
                    name: r.get(1)?,
                    kind: DefKind::parse(&kind).unwrap_or(DefKind::Symbol),
                },
                r.get::<_, String>(3)?,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn all_mentions(&self) -> Result<Vec<(Mention, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT unit_id, name, kind, line, path FROM mentions ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| {
            let kind: String = r.get(2)?;
            Ok((
                Mention {
                    unit_id: r.get(0)?,
                    name: r.get(1)?,
                    kind: MentionKind::parse(&kind).unwrap_or(MentionKind::Call),
                    line: r.get::<_, i64>(3)? as usize,
                },
                r.get::<_, String>(4)?,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn edges_from(&self, src: &str) -> Result<Vec<Edge>> {
        self.edges_where("src", src)
    }

    pub fn edges_to(&self, dst: &str) -> Result<Vec<Edge>> {
        self.edges_where("dst", dst)
    }

    fn edges_where(&self, col: &str, id: &str) -> Result<Vec<Edge>> {
        let sql = format!(
            "SELECT src, dst, kind, origin, weight, line FROM edges WHERE {col}=?1 \
             ORDER BY weight DESC, src, dst"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([id], |r| {
            let kind: String = r.get(2)?;
            Ok(Edge {
                src: r.get(0)?,
                dst: r.get(1)?,
                kind: MentionKind::parse(&kind).unwrap_or(MentionKind::Call),
                origin: r.get(3)?,
                weight: r.get(4)?,
                line: r.get::<_, i64>(5)? as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----------------------------------------------------------------- units

    pub fn unit(&self, id: &str) -> Result<Option<UnitRow>> {
        let mut stmt = self.conn.prepare_cached(&format!("{UNIT_SELECT} WHERE id=?1"))?;
        Ok(stmt.query_row([id], unit_from_row).optional()?)
    }

    pub fn children(&self, id: &str) -> Result<Vec<UnitRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{UNIT_SELECT} WHERE parent=?1 ORDER BY ord"))?;
        let rows = stmt.query_map([id], unit_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn units_in_file(&self, path: &str) -> Result<Vec<UnitRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{UNIT_SELECT} WHERE path=?1 ORDER BY ord"))?;
        let rows = stmt.query_map([path], unit_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The unit plus all of its descendants.
    pub fn subtree(&self, id: &str) -> Result<Vec<UnitRow>> {
        let mut out = Vec::new();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if let Some(u) = self.unit(&cur)? {
                for c in self.children(&cur)?.into_iter().rev() {
                    stack.push(c.id.clone());
                }
                out.push(u);
            }
        }
        Ok(out)
    }

    /// Ancestors from parent up to the file unit.
    pub fn ancestors(&self, id: &str) -> Result<Vec<UnitRow>> {
        let mut out = Vec::new();
        let mut cur = self.unit(id)?.and_then(|u| u.parent);
        while let Some(pid) = cur {
            match self.unit(&pid)? {
                Some(u) => {
                    cur = u.parent.clone();
                    out.push(u);
                }
                None => break,
            }
        }
        Ok(out)
    }

    /// Resolve a user-supplied reference: an exact unit ID, a file path, or a
    /// unique label/suffix match.
    pub fn find_unit(&self, reference: &str) -> Result<Vec<UnitRow>> {
        if let Some(u) = self.unit(reference)? {
            return Ok(vec![u]);
        }
        let mut stmt = self.conn.prepare_cached(&format!(
            "{UNIT_SELECT} WHERE label=?1 OR id LIKE ?2 ESCAPE '\\' ORDER BY length(id), id LIMIT 50"
        ))?;
        let escaped = reference.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let rows = stmt.query_map(
            params![reference, format!("%::{escaped}")],
            unit_from_row,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Units defining a code symbol with this name (case-insensitive).
    pub fn symbol_definitions(&self, name: &str) -> Result<Vec<(UnitId, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT unit_id, path FROM definitions \
             WHERE kind='symbol' AND name = ?1 COLLATE NOCASE ORDER BY unit_id",
        )?;
        let rows = stmt.query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ------------------------------------------------------------- postings

    /// Postings for a term (leaves only): `(unit_id, tf, len, path)`.
    pub fn postings(&self, term: &str) -> Result<Vec<(UnitId, f64, f64, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.unit_id, p.tf, u.len, p.path FROM postings p JOIN units u ON u.id = p.unit_id \
             WHERE p.term=?1",
        )?;
        let rows =
            stmt.query_map([term], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn stats(&self) -> Result<Stats> {
        let c = &self.conn;
        let count = |sql: &str| -> Result<usize> {
            Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as usize)
        };
        Ok(Stats {
            files: count("SELECT COUNT(*) FROM files")?,
            units: count("SELECT COUNT(*) FROM units")?,
            leaves: count("SELECT COUNT(*) FROM units WHERE is_leaf=1")?,
            avg_len: c.query_row(
                "SELECT COALESCE(AVG(len), 0) FROM units WHERE is_leaf=1",
                [],
                |r| r.get(0),
            )?,
            definitions: count("SELECT COUNT(*) FROM definitions")?,
            mentions: count("SELECT COUNT(*) FROM mentions")?,
            edges: count("SELECT COUNT(*) FROM edges")?,
        })
    }

    // ---------------------------------------------------------------- git

    pub fn replace_history(&mut self, commits: &[CommitRow]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM commits", [])?;
        tx.execute("DELETE FROM hunks", [])?;
        {
            let mut c = tx.prepare("INSERT OR REPLACE INTO commits(id, ts, subject, seq) VALUES(?1,?2,?3,?4)")?;
            let mut h = tx.prepare("INSERT INTO hunks(commit_id, path, start_line, len) VALUES(?1,?2,?3,?4)")?;
            for (seq, (id, ts, subject, hunks)) in commits.iter().enumerate() {
                c.execute(params![id, ts, subject, seq as i64])?;
                for (path, start, len) in hunks {
                    h.execute(params![id, path, *start as i64, *len as i64])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------- inspection

    /// Canonical, order-independent dump of all evidence and derived edges.
    /// Used to check that incremental indexing equals indexing from scratch.
    pub fn canonical_dump(&self) -> Result<String> {
        let mut out = BTreeMap::new();
        for (name, sql) in [
            ("files", "SELECT path, hash, size FROM files"),
            ("units", "SELECT id, COALESCE(parent,''), kind, path, start_line, end_line, label, text, is_leaf, len, ord FROM units"),
            ("postings", "SELECT term, unit_id, tf FROM postings"),
            ("definitions", "SELECT unit_id, name, kind FROM definitions"),
            ("mentions", "SELECT unit_id, name, kind, line FROM mentions"),
            ("edges", "SELECT src, dst, kind, origin, printf('%.9f', weight), line FROM edges"),
        ] {
            let mut stmt = self.conn.prepare(sql)?;
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |r| {
                    let mut cols = Vec::with_capacity(n);
                    for i in 0..n {
                        let v: rusqlite::types::Value = r.get(i)?;
                        cols.push(format!("{v:?}"));
                    }
                    Ok(cols.join("\t"))
                })?
                .collect::<Result<_, _>>()?;
            rows.sort();
            out.insert(name, rows.join("\n"));
        }
        Ok(out
            .into_iter()
            .map(|(k, v)| format!("## {k}\n{v}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

const UNIT_SELECT: &str =
    "SELECT id, parent, kind, path, start_line, end_line, label, text, is_leaf, len, ord FROM units";

fn unit_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UnitRow> {
    let kind: String = r.get(2)?;
    Ok(UnitRow {
        id: r.get(0)?,
        parent: r.get(1)?,
        kind: UnitKind::parse(&kind).unwrap_or(UnitKind::Gap),
        path: r.get(3)?,
        start_line: r.get::<_, i64>(4)? as usize,
        end_line: r.get::<_, i64>(5)? as usize,
        label: r.get(6)?,
        text: r.get(7)?,
        is_leaf: r.get::<_, i64>(8)? != 0,
        len: r.get(9)?,
        ord: r.get(10)?,
    })
}

fn delete_file_rows(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<()> {
    for table in ["postings", "definitions", "mentions", "units", "files"] {
        tx.execute(&format!("DELETE FROM {table} WHERE path=?1"), [path])?;
    }
    Ok(())
}

fn insert_extracted(tx: &rusqlite::Transaction<'_>, ex: &Extracted) -> Result<()> {
    let has_children: std::collections::HashSet<&str> =
        ex.units.iter().filter_map(|u| u.parent.as_deref()).collect();
    let mut unit_stmt = tx.prepare(
        "INSERT OR REPLACE INTO units(id, parent, kind, path, start_line, end_line, label, text, is_leaf, len, ord) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )?;
    let mut post_stmt =
        tx.prepare("INSERT INTO postings(term, unit_id, tf, path) VALUES(?1,?2,?3,?4)")?;
    for (ord, u) in ex.units.iter().enumerate() {
        let is_leaf = !has_children.contains(u.id.as_str());
        let (terms, len) = if is_leaf {
            // Index the label with the body so a symbol is found by its name.
            let tokens = tokenize(&format!("{}\n{}", u.label, u.text));
            let len = tokens.len() as f64;
            let mut tf: HashMap<String, f64> = HashMap::new();
            for t in tokens {
                *tf.entry(t).or_default() += 1.0;
            }
            let mut terms: Vec<_> = tf.into_iter().collect();
            terms.sort_by(|a, b| a.0.cmp(&b.0));
            (terms, len)
        } else {
            (Vec::new(), 0.0)
        };
        unit_stmt.execute(params![
            u.id,
            u.parent,
            u.kind.as_str(),
            ex.path,
            u.start_line as i64,
            u.end_line as i64,
            u.label,
            u.text,
            is_leaf as i64,
            len,
            ord as i64
        ])?;
        for (term, tf) in terms {
            post_stmt.execute(params![term, u.id, tf, ex.path])?;
        }
    }
    let mut def_stmt =
        tx.prepare("INSERT INTO definitions(unit_id, name, kind, path) VALUES(?1,?2,?3,?4)")?;
    for d in &ex.definitions {
        def_stmt.execute(params![d.unit_id, d.name, d.kind.as_str(), ex.path])?;
    }
    let mut men_stmt =
        tx.prepare("INSERT INTO mentions(unit_id, name, kind, line, path) VALUES(?1,?2,?3,?4,?5)")?;
    for m in &ex.mentions {
        men_stmt.execute(params![m.unit_id, m.name, m.kind.as_str(), m.line as i64, ex.path])?;
    }
    Ok(())
}
