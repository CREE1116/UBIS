//! Unit tree builder shared by all extractors.
//!
//! Extractors add named units with spans; [`UnitTree::finish`] then
//!
//! 1. makes IDs unique (`#2`, `#3`, … in document order),
//! 2. fills every container's uncovered lines with `Gap` leaves so that each
//!    non-blank line of the file belongs to exactly one leaf,
//! 3. stores full text for leaves and only the header line for containers.

use std::collections::{BTreeMap, HashMap, HashSet};

use ubis_core::model::*;

/// Gap leaves longer than this are split at blank lines (or hard-split).
pub const MAX_GAP_LINES: usize = 60;

pub struct UnitTree<'a> {
    pub path: String,
    lines: Vec<&'a str>,
    units: Vec<Unit>,
    pub definitions: Vec<Definition>,
    pub mentions: Vec<Mention>,
}

impl<'a> UnitTree<'a> {
    pub fn new(path: &str, content: &'a str) -> Self {
        let lines: Vec<&str> = content.lines().collect();
        let n = lines.len().max(1);
        let label = path.rsplit('/').next().unwrap_or(path).to_string();
        let root = Unit {
            id: path.to_string(),
            parent: None,
            kind: UnitKind::File,
            path: path.to_string(),
            start_line: 1,
            end_line: n,
            label,
            text: String::new(),
        };
        Self {
            path: path.to_string(),
            lines,
            units: vec![root],
            definitions: Vec::new(),
            mentions: Vec::new(),
        }
    }

    pub fn root_id(&self) -> String {
        self.path.clone()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, one_based: usize) -> &'a str {
        self.lines.get(one_based.wrapping_sub(1)).copied().unwrap_or("")
    }

    /// Add a unit. Returns the ID actually assigned (deduplicated at finish;
    /// callers should use the returned ID as a parent for children).
    pub fn add(
        &mut self,
        parent: &str,
        id: String,
        kind: UnitKind,
        start_line: usize,
        end_line: usize,
        label: String,
    ) -> String {
        let end = end_line.max(start_line).min(self.lines.len().max(1));
        let start = start_line.max(1).min(end);
        // Dedupe eagerly so children can reference the final ID.
        let mut final_id = id.clone();
        let mut n = 2;
        while self.units.iter().any(|u| u.id == final_id) {
            final_id = format!("{id}#{n}");
            n += 1;
        }
        self.units.push(Unit {
            id: final_id.clone(),
            parent: Some(parent.to_string()),
            kind,
            path: self.path.clone(),
            start_line: start,
            end_line: end,
            label,
            text: String::new(),
        });
        final_id
    }

    pub fn define(&mut self, unit_id: &str, name: &str, kind: DefKind) {
        self.definitions.push(Definition {
            unit_id: unit_id.to_string(),
            name: name.to_string(),
            kind,
        });
    }

    pub fn mention(&mut self, unit_id: &str, name: &str, kind: MentionKind, line: usize) {
        if name.is_empty() {
            return;
        }
        self.mentions.push(Mention {
            unit_id: unit_id.to_string(),
            name: name.to_string(),
            kind,
            line,
        });
    }

    /// Innermost unit whose span contains `line` (ties → the later-added,
    /// i.e. deeper, unit).
    pub fn innermost(&self, line: usize) -> String {
        let mut best: Option<&Unit> = None;
        for u in &self.units {
            if u.start_line <= line && line <= u.end_line {
                let better = match best {
                    None => true,
                    Some(b) => (u.end_line - u.start_line) <= (b.end_line - b.start_line),
                };
                if better {
                    best = Some(u);
                }
            }
        }
        best.map(|u| u.id.clone()).unwrap_or_else(|| self.root_id())
    }

    fn span_text(&self, start: usize, end: usize) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        self.lines[start - 1..end.min(self.lines.len())].join("\n")
    }

    pub fn finish(mut self) -> Extracted {
        // Children per parent, in document order.
        let mut children: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
        for u in &self.units {
            if let Some(p) = &u.parent {
                children
                    .entry(p.clone())
                    .or_default()
                    .push((u.start_line, u.end_line));
            }
        }

        // Gap leaves for uncovered, non-blank lines of each container.
        let containers: Vec<(String, UnitKind, usize, usize)> = self
            .units
            .iter()
            .filter(|u| children.contains_key(&u.id))
            .map(|u| (u.id.clone(), u.kind, u.start_line, u.end_line))
            .collect();
        let mut gaps = Vec::new();
        for (id, kind, start, end) in containers {
            let mut covered = vec![false; end + 1];
            // A container's first line is its header and is stored as the
            // container's own text; it does not need a gap leaf.
            if kind != UnitKind::File {
                covered[start] = true;
            }
            for (s, e) in &children[&id] {
                let (a, b) = ((*s).max(start), (*e).min(end));
                if a <= b {
                    covered[a..=b].fill(true);
                }
            }
            let mut runs: Vec<(usize, usize)> = Vec::new();
            let mut l = start;
            while l <= end {
                if covered[l] || is_trivial(self.line(l)) {
                    l += 1;
                    continue;
                }
                let s = l;
                // Extend over non-covered lines; allow blank lines inside.
                let mut e = l;
                while l <= end && !covered[l] {
                    if !is_trivial(self.line(l)) {
                        e = l;
                    }
                    l += 1;
                }
                runs.extend(split_run(&self.lines, s, e));
            }
            for (n, (s, e)) in runs.into_iter().enumerate() {
                gaps.push((id.clone(), format!("{id}/~{}", n + 1), s, e));
            }
        }
        for (parent, gid, s, e) in gaps {
            let label = format!("{} ~{}", self.label_of(&parent), gid.rsplit('~').next().unwrap_or(""));
            self.add(&parent, gid, UnitKind::Gap, s, e, label);
        }

        // Sort units by (start, -span) so `ord` follows document order with
        // parents before children.
        let root = self.units.remove(0);
        self.units.sort_by(|a, b| {
            a.start_line
                .cmp(&b.start_line)
                .then_with(|| b.end_line.cmp(&a.end_line))
                .then_with(|| a.id.cmp(&b.id))
        });
        self.units.insert(0, root);

        let parents: HashSet<String> = self.units.iter().filter_map(|u| u.parent.clone()).collect();
        let texts: HashMap<String, String> = self
            .units
            .iter()
            .map(|u| {
                let t = if parents.contains(&u.id) {
                    if u.kind == UnitKind::File {
                        String::new()
                    } else {
                        self.line(u.start_line).to_string()
                    }
                } else {
                    self.span_text(u.start_line, u.end_line)
                };
                (u.id.clone(), t)
            })
            .collect();
        for u in &mut self.units {
            u.text = texts[&u.id].clone();
        }

        self.mentions.sort_by(|a, b| {
            (a.line, &a.unit_id, &a.name, a.kind).cmp(&(b.line, &b.unit_id, &b.name, b.kind))
        });
        Extracted {
            path: self.path,
            units: self.units,
            definitions: self.definitions,
            mentions: self.mentions,
        }
    }

    fn label_of(&self, id: &str) -> String {
        self.units
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.label.clone())
            .unwrap_or_default()
    }
}

/// Blank lines and lines holding only closing punctuation (`}`, `);`) carry
/// no content of their own and never form a gap leaf.
pub fn is_trivial(line: &str) -> bool {
    line.trim().chars().all(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ','))
}

/// Split a gap run into pieces of at most `MAX_GAP_LINES`, preferring blank
/// lines as cut points. Returned spans are trimmed of blank edges.
fn split_run(lines: &[&str], s: usize, e: usize) -> Vec<(usize, usize)> {
    let blank = |l: usize| lines.get(l - 1).is_none_or(|t| is_trivial(t));
    let mut out = Vec::new();
    let mut start = s;
    while start <= e {
        let mut end = (start + MAX_GAP_LINES - 1).min(e);
        if end < e {
            // Prefer the last blank line in the window.
            if let Some(b) = (start + 1..=end).rev().find(|&l| blank(l)) {
                end = b - 1;
            }
        }
        let (mut a, mut b) = (start, end);
        while a <= b && blank(a) {
            a += 1;
        }
        while b >= a && blank(b) {
            b -= 1;
        }
        if a <= b {
            out.push((a, b));
        }
        start = end + 1;
        while start <= e && blank(start) {
            start += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_cover_uncovered_lines() {
        let src = "use a;\n\nfn f() {\n}\n\nconst X: u8 = 1;\n";
        let mut t = UnitTree::new("x.rs", src);
        let root = t.root_id();
        t.add(&root, "x.rs::f".into(), UnitKind::Function, 3, 4, "f".into());
        let ex = t.finish();
        let spans: Vec<_> = ex
            .units
            .iter()
            .map(|u| (u.id.as_str(), u.start_line, u.end_line))
            .collect();
        assert_eq!(
            spans,
            vec![("x.rs", 1, 6), ("x.rs/~1", 1, 1), ("x.rs::f", 3, 4), ("x.rs/~2", 6, 6)]
        );
        // Gap runs: line 1 and line 6 are separate runs split by covered lines.
        let gaps: Vec<_> = ex.units.iter().filter(|u| u.kind == UnitKind::Gap).collect();
        assert_eq!(gaps.len(), 2);
        assert_eq!((gaps[0].start_line, gaps[0].end_line), (1, 1));
        assert_eq!((gaps[1].start_line, gaps[1].end_line), (6, 6));
        assert_eq!(gaps[1].text, "const X: u8 = 1;");
        // Container keeps only its header; leaf keeps its body.
        assert_eq!(ex.units[0].text, "");
        let f = ex.units.iter().find(|u| u.id == "x.rs::f").unwrap();
        assert_eq!(f.text, "fn f() {\n}");
    }

    #[test]
    fn duplicate_ids_get_suffix() {
        let mut t = UnitTree::new("a.md", "a\nb\n");
        let r = t.root_id();
        let a = t.add(&r, "a.md#x".into(), UnitKind::Section, 1, 1, "x".into());
        let b = t.add(&r, "a.md#x".into(), UnitKind::Section, 2, 2, "x".into());
        assert_eq!((a.as_str(), b.as_str()), ("a.md#x", "a.md#x#2"));
    }

    #[test]
    fn long_gaps_split_at_blank_lines() {
        let mut src = String::new();
        for i in 0..100 {
            src.push_str(&format!("line {i}\n"));
            if i == 49 {
                src.push('\n');
            }
        }
        let t = UnitTree::new("t.txt", &src);
        let ex = t.finish();
        // A file with no children is itself a leaf.
        assert_eq!(ex.units.len(), 1);
        let pieces = split_run(&src.lines().collect::<Vec<_>>(), 1, 101);
        assert_eq!(pieces, vec![(1, 50), (52, 101)]);
    }
}
