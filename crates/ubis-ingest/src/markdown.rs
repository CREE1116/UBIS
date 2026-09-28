//! Markdown extractor.
//!
//! * Sections from ATX headings form the tree (`path#install/linux`).
//! * Section bodies split into paragraph and fenced-code leaves.
//! * Headings define anchors named like GitHub renders them (`path#slug`,
//!   duplicates get `-1`, `-2`), so links written by humans resolve.
//! * Inline links (`[t](#a)`, `[t](other.md#a)`), wiki links (`[[Note]]`), and
//!   identifiers in prose become mentions.

use std::collections::HashMap;

use ubis_core::model::*;

use crate::prose::{bridge_mentions, paragraphs};
use crate::tree::UnitTree;

pub const MAX_PARAGRAPH_LINES: usize = 40;

struct Heading {
    line: usize,
    level: usize,
    text: String,
}

pub fn extract(path: &str, content: &str) -> Extracted {
    let mut tree = UnitTree::new(path, content);
    let n = tree.line_count();
    let root = tree.root_id();
    tree.define(&root, path, DefKind::Anchor);

    // Pass 1: headings and fenced blocks.
    let mut headings = Vec::new();
    let mut fences: Vec<(usize, usize)> = Vec::new();
    let mut fence: Option<(usize, String)> = None;
    for l in 1..=n {
        let line = tree.line(l);
        let t = line.trim_start();
        if let Some((start, marker)) = &fence {
            if t.starts_with(marker.as_str()) {
                fences.push((*start, l));
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some((l, t[..3].to_string()));
            continue;
        }
        if let Some(h) = parse_heading(line) {
            headings.push(Heading {
                line: l,
                level: h.0,
                text: h.1,
            });
        }
    }
    if let Some((start, _)) = fence {
        fences.push((start, n));
    }
    let in_fence = |l: usize| fences.iter().any(|(s, e)| *s <= l && l <= *e);

    // Pass 2: sections.
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut stack: Vec<(usize, String, String)> = Vec::new(); // (level, id, slug path)
    let mut bodies: Vec<(String, String, usize, usize)> = Vec::new(); // (id, label, body start, body end)
    let first_heading = headings.first().map(|h| h.line).unwrap_or(n + 1);
    if first_heading > 1 {
        bodies.push((root.clone(), String::new(), 1, first_heading - 1));
    }
    for (i, h) in headings.iter().enumerate() {
        let end = headings[i + 1..]
            .iter()
            .find(|o| o.level <= h.level)
            .map(|o| o.line - 1)
            .unwrap_or(n);
        while stack.last().is_some_and(|(lvl, _, _)| *lvl >= h.level) {
            stack.pop();
        }
        let slug = slugify(&h.text);
        let (parent, slug_path) = match stack.last() {
            Some((_, pid, sp)) => (pid.clone(), format!("{sp}/{slug}")),
            None => (root.clone(), slug.clone()),
        };
        let id = tree.add(
            &parent,
            format!("{path}#{slug_path}"),
            UnitKind::Section,
            h.line,
            end,
            h.text.clone(),
        );
        // GitHub-style anchor with duplicate numbering.
        let count = slug_counts.entry(slug.clone()).or_insert(0);
        let anchor = if *count == 0 {
            slug.clone()
        } else {
            format!("{slug}-{count}")
        };
        *count += 1;
        tree.define(&id, &format!("{path}#{anchor}"), DefKind::Anchor);

        let body_end = headings
            .get(i + 1)
            .map(|o| o.line - 1)
            .unwrap_or(n)
            .min(end);
        bodies.push((id.clone(), h.text.clone(), h.line + 1, body_end));
        stack.push((h.level, id, slug_path));
    }

    // Pass 3: body leaves (paragraphs and fenced code).
    for (parent, label, start, end) in bodies {
        if start > end {
            continue;
        }
        let mut p_no = 0;
        let mut c_no = 0;
        let mut l = start;
        while l <= end {
            if let Some((fs, fe)) = fences.iter().find(|(s, _)| *s == l).copied() {
                let fe = fe.min(end);
                c_no += 1;
                let lbl = if label.is_empty() {
                    format!("code {c_no}")
                } else {
                    format!("{label} · code {c_no}")
                };
                tree.add(&parent, format!("{parent}/code{c_no}"), UnitKind::CodeBlock, fs, fe, lbl);
                l = fe + 1;
                continue;
            }
            // Prose run up to the next fence.
            let next_fence = fences
                .iter()
                .map(|(s, _)| *s)
                .filter(|s| *s > l && *s <= end)
                .min()
                .unwrap_or(end + 1);
            for (a, b) in paragraphs(&tree, l, next_fence - 1, MAX_PARAGRAPH_LINES) {
                p_no += 1;
                let lbl = if label.is_empty() {
                    format!("¶{p_no}")
                } else {
                    format!("{label} ¶{p_no}")
                };
                tree.add(&parent, format!("{parent}/¶{p_no}"), UnitKind::Paragraph, a, b, lbl);
            }
            l = next_fence;
        }
    }

    // Pass 4: mentions on non-fenced lines, attributed to the innermost unit.
    let dir = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    for l in 1..=n {
        if in_fence(l) {
            continue;
        }
        let line = tree.line(l);
        let unit = tree.innermost(l);
        for target in links(line) {
            if let Some(name) = normalize_link(path, dir, &target) {
                tree.mention(&unit, &name, MentionKind::Link, l);
            }
        }
        bridge_mentions(&mut tree, &unit, l, line);
    }

    tree.finish()
}

fn parse_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim().to_string();
    (!text.is_empty()).then_some((level, text))
}

/// GitHub-style heading slug: lowercase, drop punctuation except `-`/`_`,
/// spaces become `-`. Unicode letters (e.g. Hangul) are kept.
pub fn slugify(text: &str) -> String {
    let mut s = String::new();
    for c in text.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            s.extend(c.to_lowercase());
        } else if c == ' ' {
            s.push('-');
        }
    }
    s
}

/// Link targets on one line: `[text](target)` (not images) and `[[wiki]]`.
fn links(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut code = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => code = !code,
            b'[' if !code && bytes.get(i + 1) == Some(&b'[') => {
                if let Some(end) = line[i + 2..].find("]]") {
                    let inner = &line[i + 2..i + 2 + end];
                    let name = inner.split('|').next().unwrap_or(inner).trim();
                    if !name.is_empty() {
                        out.push(format!("wiki:{name}"));
                    }
                    i += end + 4;
                    continue;
                }
            }
            b']' if !code && bytes.get(i + 1) == Some(&b'(') => {
                let is_image = line[..i]
                    .rfind('[')
                    .is_some_and(|o| o > 0 && bytes[o - 1] == b'!');
                if let Some(close) = line[i + 2..].find(')') {
                    let target = line[i + 2..i + 2 + close].split_whitespace().next().unwrap_or("");
                    if !is_image && !target.is_empty() {
                        out.push(target.to_string());
                    }
                    i += close + 3;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Normalize a link to the anchor-definition naming scheme, or `None` for
/// external links.
fn normalize_link(path: &str, dir: &str, target: &str) -> Option<String> {
    if let Some(name) = target.strip_prefix("wiki:") {
        let file = if name.ends_with(".md") {
            name.to_string()
        } else {
            format!("{name}.md")
        };
        return Some(join(dir, &file));
    }
    if target.contains("://") || target.starts_with("mailto:") {
        return None;
    }
    let (file, anchor) = match target.split_once('#') {
        Some((f, a)) => (f, Some(a)),
        None => (target, None),
    };
    let file = if file.is_empty() {
        path.to_string()
    } else {
        join(dir, &percent_decode(file))
    };
    Some(match anchor {
        Some(a) if !a.is_empty() => format!("{file}#{}", percent_decode(a).to_lowercase()),
        _ => file,
    })
}

fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if rel.starts_with('/') || dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in rel.trim_start_matches('/').split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
Intro line about `HnswIndex`.

# Install

Run it. See [details](#details) and [other](../guide/setup.md#first-step).

```bash
cargo build [not a link](#x)
```

## Details

Body.

# Install

Second install section.
";

    #[test]
    fn sections_paragraphs_code() {
        let ex = extract("docs/a.md", DOC);
        let ids: Vec<_> = ex.units.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "docs/a.md",
                "docs/a.md/¶1",
                "docs/a.md#install",
                "docs/a.md#install/¶1",
                "docs/a.md#install/code1",
                "docs/a.md#install/details",
                "docs/a.md#install/details/¶1",
                "docs/a.md#install#2",
                "docs/a.md#install#2/¶1",
            ]
        );
        let code = ex.units.iter().find(|u| u.kind == UnitKind::CodeBlock).unwrap();
        assert_eq!((code.start_line, code.end_line), (7, 9));
    }

    #[test]
    fn anchors_follow_github_numbering() {
        let ex = extract("docs/a.md", DOC);
        let anchors: Vec<_> = ex.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            anchors,
            vec!["docs/a.md", "docs/a.md#install", "docs/a.md#details", "docs/a.md#install-1"]
        );
    }

    #[test]
    fn links_skip_code_and_normalize() {
        let ex = extract("docs/a.md", DOC);
        let links: Vec<_> = ex
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Link)
            .map(|m| (m.unit_id.as_str(), m.name.as_str()))
            .collect();
        assert_eq!(
            links,
            vec![
                ("docs/a.md#install/¶1", "docs/a.md#details"),
                ("docs/a.md#install/¶1", "guide/setup.md#first-step"),
            ]
        );
        let bridges: Vec<_> = ex
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Bridge)
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(bridges, vec!["HnswIndex"]);
    }

    #[test]
    fn korean_slugs_and_wiki_links() {
        assert_eq!(slugify("설치 방법 (macOS)"), "설치-방법-macos");
        let ex = extract("n/a.md", "# 제목\n[[Other Note]] and [x](b.md)\n");
        let links: Vec<_> = ex
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Link)
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(links, vec!["n/Other Note.md", "n/b.md"]);
    }
}
