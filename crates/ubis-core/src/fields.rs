//! Searchable fields and the one scoring function over them.
//!
//! A unit is described by several texts, each derived from evidence:
//!
//! | field     | document | text                                                  |
//! |-----------|----------|-------------------------------------------------------|
//! | `code`    | leaf     | label + body (the `postings` table)                   |
//! | `path`    | leaf     | path words                                            |
//! | `name`    | leaf     | identifiers of the unit ID + file stem                |
//! | `history` | leaf     | messages of the commits that changed it               |
//! | `tests`   | file     | test lines that mention a name the file defines       |
//!
//! Each field is scored with DPH (Divergence From Randomness, parameter
//! free): the bits by which a term's frequency in a document departs from
//! chance. Fields are independent evidence, so their bits add; no field
//! weights. Measured on SWE-bench (UBIS-V2 lab, held-out repos).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{Definition, DefKind, Mention, MentionKind, UnitId};
use crate::tokenize::tokenize;

pub const PATH: &str = "path";
pub const NAME: &str = "name";
pub const TESTS: &str = "tests";

/// DPH weight of one term in one document (Amati; Terrier's form).
/// `tf` term frequency, `dl` document length, `avg` mean length over all
/// documents of the field, `n` number of documents, `cf` collection frequency.
pub fn dph(tf: f64, dl: f64, avg: f64, n: f64, cf: f64) -> f64 {
    if tf <= 0.0 || dl <= 0.0 || cf <= 0.0 {
        return 0.0;
    }
    let f = tf / dl;
    let norm = (1.0 - f) * (1.0 - f) / (tf + 1.0);
    norm * (tf * ((tf * avg / dl) * (n / cf)).log2()
        + 0.5 * (2.0 * std::f64::consts::PI * tf * (1.0 - f).max(1e-12)).log2())
}

/// Path words: `src/exec/job.rs` → `src exec job rs`.
pub fn path_text(path: &str) -> String {
    path.replace(['/', '.'], " ")
}

/// Identifiers of the unit ID below the file, plus the file stem:
/// `src/a.rs::Store::open` → `Store open a`.
pub fn name_text(id: &str, path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    if let Some(rest) = id.strip_prefix(path).and_then(|r| r.strip_prefix("::")) {
        out.extend(identifiers(rest));
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split_once('.').map_or(file, |(s, _)| s);
    out.push(stem);
    out.join(" ")
}

fn identifiers(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_'))
}

/// Test code by path convention (`tests/`, `test_x.py`, `x_test.go`,
/// `x.test.ts`, `conftest.py`) or an inline Rust `mod tests`.
pub fn is_test(id: &str, path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(p) = parts.next() {
        let last = parts.peek().is_none();
        if matches!(p, "test" | "tests" | "testing" | "__tests__" | "spec" | "specs") {
            return true;
        }
        if last {
            let (stem, _) = p.rsplit_once('.').unwrap_or((p, ""));
            if stem.starts_with("test_")
                || stem.ends_with("_test")
                || stem.ends_with(".test")
                || stem.ends_with(".spec")
                || stem.ends_with("_spec")
                || stem == "conftest"
            {
                return true;
            }
        }
    }
    id.contains("::tests::")
}

/// A mentioning unit, as the tests field needs it.
pub struct Mentioner<'a> {
    pub label: &'a str,
    pub text: &'a str,
    pub start_line: usize,
}

/// The `tests` field: for every line of test code that mentions a name
/// defined in the project, that line's terms plus the test's label go to the
/// file defining the name. A name defined by `m` units gives each `1/m`
/// (ambiguity is split, not dropped). Aggregated per file: tests describe
/// the behaviour of the file they exercise; per unit they reward units that
/// are merely called often (measured: unit-level lost, file-level won).
pub fn test_field(
    defs: &[(Definition, String)],
    mentions: &[(Mention, String)],
    units: &HashMap<UnitId, Mentioner<'_>>,
) -> BTreeMap<String, BTreeMap<String, f64>> {
    // Plain name → defining units (and their files).
    let mut definers: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
    for (d, path) in defs {
        if d.kind != DefKind::Symbol || d.name.contains("::") || d.name.chars().count() < 3 {
            continue;
        }
        if d.name.starts_with("__") && d.name.ends_with("__") {
            continue;
        }
        definers.entry(d.name.as_str()).or_default().insert(d.unit_id.as_str(), path.as_str());
    }
    // (mentioning unit, line) → distinct names.
    let mut lines: BTreeMap<(&str, usize), BTreeSet<&str>> = BTreeMap::new();
    for (m, path) in mentions {
        if !matches!(m.kind, MentionKind::Call | MentionKind::Method | MentionKind::Import | MentionKind::Type) {
            continue;
        }
        if !is_test(&m.unit_id, path) {
            continue;
        }
        let name = m.name.rsplit("::").next().unwrap_or(&m.name);
        if definers.contains_key(name) {
            lines.entry((m.unit_id.as_str(), m.line)).or_default().insert(name);
        }
    }
    let mut out: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for ((unit, line), names) in lines {
        let Some(u) = units.get(unit) else { continue };
        let text = u.text.lines().nth(line.saturating_sub(u.start_line)).unwrap_or("");
        let mut terms = tokenize(text);
        terms.extend(tokenize(u.label));
        for name in names {
            let ds = &definers[name];
            let w = 1.0 / ds.len() as f64;
            for (&target, &file) in ds {
                if target == unit {
                    continue;
                }
                let doc = out.entry(file.to_string()).or_default();
                for t in &terms {
                    *doc.entry(t.clone()).or_default() += w;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert_eq!(name_text("src/a.rs::Store::open", "src/a.rs"), "Store open a");
        assert_eq!(name_text("src/a.rs/~1", "src/a.rs"), "a");
        assert_eq!(path_text("src/exec/job.rs"), "src exec job rs");
    }

    #[test]
    fn test_paths() {
        for p in ["tests/a.py", "pkg/tests/x.rs", "test_a.py", "a/b_test.go", "web/x.test.ts", "conftest.py"] {
            assert!(is_test(p, p), "{p}");
        }
        for p in ["src/testing_utils.rs", "src/attest.py", "latest.md"] {
            assert!(!is_test(p, p), "{p}");
        }
        assert!(is_test("src/a.rs::tests::works", "src/a.rs"));
    }

    #[test]
    fn dph_prefers_rare_concentrated_terms() {
        let rare = dph(2.0, 20.0, 50.0, 1000.0, 3.0);
        let common = dph(2.0, 20.0, 50.0, 1000.0, 900.0);
        assert!(rare > common && rare > 0.0);
        assert_eq!(dph(0.0, 10.0, 10.0, 10.0, 1.0), 0.0);
    }
}
