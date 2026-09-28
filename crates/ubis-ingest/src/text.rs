//! Plain-text extractor: paragraph leaves plus identifier bridges. Also used
//! for code in languages without a tree-sitter extractor yet.

use ubis_core::model::*;

use crate::prose::{bridge_mentions, paragraphs};
use crate::tree::UnitTree;

pub const MAX_PARAGRAPH_LINES: usize = 40;

pub fn extract(path: &str, content: &str, prose: bool) -> Extracted {
    let mut tree = UnitTree::new(path, content);
    let root = tree.root_id();
    tree.define(&root, path, DefKind::Anchor);
    let n = tree.line_count();
    let paras = paragraphs(&tree, 1, n, MAX_PARAGRAPH_LINES);
    if paras.len() > 1 {
        for (i, (a, b)) in paras.into_iter().enumerate() {
            let id = format!("{path}/¶{}", i + 1);
            tree.add(&root, id, UnitKind::Paragraph, a, b, format!("¶{}", i + 1));
        }
    }
    if prose {
        for l in 1..=n {
            let unit = tree.innermost(l);
            let line = tree.line(l);
            bridge_mentions(&mut tree, &unit, l, line);
        }
    }
    tree.finish()
}
