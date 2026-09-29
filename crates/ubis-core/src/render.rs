//! Text rendering of results. This is what an agent actually reads, so its
//! size is part of the cost the index exists to cut (measured by ubis-bench
//! as `list_tok`). Two lines per hit:
//!
//! ```text
//! src/exec/job.rs:7-43 job · cochange tree_near
//!   /// An event loop that listens for inputs from the `rx` receiver…
//! ```
//!
//! `path:start-end` is both what to read and a valid unit reference for
//! `near`/`open`, so the full ID is not repeated.

use crate::query::{Hit, Response};

/// Source list, then the test list (when any) under `tests:`.
pub fn response(r: &Response) -> String {
    let mut s = hits(&r.hits);
    if !r.tests.is_empty() {
        s += "tests:\n";
        s += &hits(&r.tests);
    }
    s
}

/// Candidate list as printed by `ubis find` / `ubis near`.
pub fn hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no candidates\n".into();
    }
    let mut s = String::new();
    for h in hits {
        let mut via: Vec<_> = h.via.iter().collect();
        via.sort_by(|a, b| b.contribution.total_cmp(&a.contribution).then_with(|| a.op.cmp(b.op)));
        let via: Vec<&str> = via.iter().map(|v| v.op).collect();
        s += &format!("{}:{}-{} {}", h.path, h.start_line, h.end_line, short_name(&h.id, &h.path));
        if h.lifted > 0 {
            s += &format!(" (+{} inside)", h.lifted);
        }
        s += &format!(" · {}\n", via.join(" "));
        if !h.signature.is_empty() {
            s += &format!("  {}\n", h.signature);
        }
    }
    s
}

/// The ID without its path: `Store::open`, `install/¶2`, or `(file)`.
fn short_name<'a>(id: &'a str, path: &str) -> &'a str {
    let rest = id.strip_prefix(path).unwrap_or(id);
    if let Some(r) = rest.strip_prefix("::") {
        return r;
    }
    let rest = rest.trim_start_matches(['#', '/']);
    if rest.is_empty() {
        return "(file)";
    }
    // Headings nest deeply; the last two levels locate the unit.
    match rest.rmatch_indices('/').nth(1) {
        Some((i, _)) => &rest[i + 1..],
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::short_name;

    #[test]
    fn short_names() {
        assert_eq!(short_name("src/a.rs::Store::open", "src/a.rs"), "Store::open");
        assert_eq!(short_name("d.md#guide/install/linux/¶2", "d.md"), "linux/¶2");
        assert_eq!(short_name("d.md#guide", "d.md"), "guide");
        assert_eq!(short_name("notes.txt", "notes.txt"), "(file)");
        assert_eq!(short_name("src/a.rs/~1", "src/a.rs"), "~1");
    }
}
