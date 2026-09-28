//! Prose helpers: identifier bridges and paragraph splitting.

use ubis_core::model::MentionKind;
use ubis_core::tokenize::is_identifier_shaped;

use crate::tree::UnitTree;

/// Record identifiers written in prose as `bridge` mentions. Two sources:
/// backticked spans (`` `Store::open` ``, `` `ef_search()` ``) and bare words
/// that look like identifiers (`HnswIndex`, `ef_search`). The mention name is
/// the last path segment, which is what definitions are keyed by.
pub fn bridge_mentions(tree: &mut UnitTree<'_>, unit_id: &str, line_no: usize, line: &str) {
    let mut names: Vec<String> = Vec::new();
    let mut rest = line;
    let mut outside = String::new();
    while let Some(open) = rest.find('`') {
        outside.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                if let Some(n) = code_span_name(&after[..close]) {
                    names.push(n);
                }
                rest = &after[close + 1..];
            }
            None => {
                rest = after;
                break;
            }
        }
    }
    outside.push_str(rest);
    for word in outside.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.')) {
        let word = word.trim_matches(|c: char| c == '.' || c == ':');
        if is_identifier_shaped(word) {
            if let Some(n) = last_segment(word) {
                names.push(n);
            }
        }
    }
    names.sort();
    names.dedup();
    for n in names {
        tree.mention(unit_id, &n, MentionKind::Bridge, line_no);
    }
}

fn code_span_name(span: &str) -> Option<String> {
    let s = span.trim().trim_end_matches("()").trim_end_matches('!');
    let s = s.split('(').next().unwrap_or(s);
    let valid = !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.')
        && s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
    if !valid {
        return None;
    }
    last_segment(s)
}

fn last_segment(s: &str) -> Option<String> {
    let seg = s.rsplit([':', '.']).find(|p| !p.is_empty())?;
    (seg.chars().count() >= 3).then(|| seg.to_string())
}

/// Split lines `[start, end]` into paragraphs separated by blank lines,
/// hard-splitting paragraphs longer than `max_lines`.
pub fn paragraphs(tree: &UnitTree<'_>, start: usize, end: usize, max_lines: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut l = start;
    while l <= end {
        if tree.line(l).trim().is_empty() {
            l += 1;
            continue;
        }
        let s = l;
        while l <= end && !tree.line(l).trim().is_empty() {
            l += 1;
        }
        let e = l - 1;
        let mut a = s;
        while a <= e {
            let b = (a + max_lines - 1).min(e);
            out.push((a, b));
            a = b + 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridges_from_backticks_and_identifiers() {
        let mut t = UnitTree::new("a.md", "x");
        bridge_mentions(
            &mut t,
            "a.md",
            1,
            "Call `Store::open()` then tune ef_search on HnswIndex. Plain words ignored.",
        );
        let names: Vec<_> = t.mentions.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["HnswIndex", "ef_search", "open"]);
    }
}
