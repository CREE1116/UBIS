//! Code extractor built on tree-sitter (Rust, Python, Java).
//!
//! Units: modules, types, impl blocks, functions, methods. IDs are structural
//! (`src/store.rs::Store::open`) so edits that shift lines keep them stable.
//! Definitions: every named unit. Mentions: calls, type references, imports.
//! Resolution to targets happens later, in `ubis_core::resolve`.

use tree_sitter::{Node, Parser};
use ubis_core::model::*;

use crate::tree::UnitTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    Java,
}

impl Lang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "java" => Some(Lang::Java),
            _ => None,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }
}

struct Ctx<'t, 's> {
    tree: UnitTree<'t>,
    src: &'s [u8],
    lang: Lang,
}

/// Scope while walking: the unit mentions belong to, and the ID prefix for
/// named children.
#[derive(Clone)]
struct Scope {
    unit: String,
    prefix: String,
    /// Directly inside a type body (impl, trait, class): functions are methods.
    in_owner: bool,
    /// Enclosing type, kept inside method bodies to resolve `Self::f()`.
    self_type: Option<String>,
}

pub fn extract(path: &str, content: &str, lang: Lang) -> anyhow::Result<Extracted> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language())?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {path}"))?;
    let mut ctx = Ctx {
        tree: UnitTree::new(path, content),
        src: content.as_bytes(),
        lang,
    };
    let root = Scope {
        unit: ctx.tree.root_id(),
        prefix: path.to_string(),
        in_owner: false,
        self_type: None,
    };
    visit(&mut ctx, tree.root_node(), &root);
    Ok(ctx.tree.finish())
}

fn text<'a>(ctx: &Ctx<'_, 'a>, node: Node<'_>) -> &'a str {
    node.utf8_text(ctx.src).unwrap_or("")
}

fn start_line(n: Node<'_>) -> usize {
    n.start_position().row + 1
}

fn end_line(n: Node<'_>) -> usize {
    let p = n.end_position();
    if p.column == 0 && p.row > n.start_position().row {
        p.row
    } else {
        p.row + 1
    }
}

/// Extend a definition upward over adjacent doc comments and attributes.
fn extended_start(ctx: &Ctx<'_, '_>, node: Node<'_>) -> usize {
    let mut start = node;
    if ctx.lang == Lang::Python {
        if let Some(p) = node.parent() {
            if p.kind() == "decorated_definition" {
                return start_line(p);
            }
        }
        return start_line(node);
    }
    let mut first = start_line(node);
    while let Some(prev) = start.prev_named_sibling() {
        let k = prev.kind();
        let attachable = matches!(k, "line_comment" | "block_comment" | "attribute_item");
        if attachable && end_line(prev) + 1 >= first {
            first = start_line(prev);
            start = prev;
        } else {
            break;
        }
    }
    first
}

fn add_named(
    ctx: &mut Ctx<'_, '_>,
    node: Node<'_>,
    scope: &Scope,
    name: &str,
    kind: UnitKind,
    label: String,
) -> String {
    let id = format!("{}::{}", scope.prefix, name);
    let s = extended_start(ctx, node);
    let id = ctx.tree.add(&scope.unit, id, kind, s, end_line(node), label);
    ctx.tree.define(&id, name, DefKind::Symbol);
    if scope.in_owner {
        if let Some(owner) = &scope.self_type {
            ctx.tree.define(&id, &format!("{owner}::{name}"), DefKind::Symbol);
        }
    }
    id
}

fn visit_children(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope) {
    let mut cursor = node.walk();
    let kids: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in kids {
        visit(ctx, child, scope);
    }
}

fn visit(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope) {
    match ctx.lang {
        Lang::Rust => visit_rust(ctx, node, scope),
        Lang::Python => visit_python(ctx, node, scope),
        Lang::Java => visit_java(ctx, node, scope),
    }
}

fn field_text(ctx: &Ctx<'_, '_>, node: Node<'_>, field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| text(ctx, n).to_string())
        .filter(|s| !s.is_empty())
}

fn is_def_name(node: Node<'_>) -> bool {
    node.parent()
        .and_then(|p| p.child_by_field_name("name"))
        .is_some_and(|n| n.id() == node.id())
}

fn mention_identifiers(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope, kind: MentionKind, last_only: bool) {
    let mut names = Vec::new();
    collect_identifiers(ctx, node, &mut names);
    if last_only {
        names = names.into_iter().last().into_iter().collect();
    }
    for (name, line) in names {
        ctx.tree.mention(&scope.unit, &name, kind, line);
    }
}

fn collect_identifiers(ctx: &Ctx<'_, '_>, node: Node<'_>, out: &mut Vec<(String, usize)>) {
    if matches!(node.kind(), "identifier" | "type_identifier" | "scoped_identifier_name") {
        out.push((text(ctx, node).to_string(), start_line(node)));
        return;
    }
    let mut cursor = node.walk();
    let kids: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for c in kids {
        collect_identifiers(ctx, c, out);
    }
}

// ---------------------------------------------------------------------- Rust

/// Callee name for a Rust call. Type-qualified paths keep their qualifier
/// (`HashMap::new`, `Self::open` → `Store::open`) so resolution does not
/// confuse `Vec::new()` with a local `new`. Module paths (`bm25::f`) are
/// lowercase by convention and resolve by simple name.
fn rust_callee(ctx: &Ctx<'_, '_>, f: Node<'_>, scope: &Scope) -> Option<String> {
    match f.kind() {
        "identifier" => Some(text(ctx, f).to_string()),
        "scoped_identifier" => {
            let name = field_text(ctx, f, "name")?;
            let qualifier = field_text(ctx, f, "path").map(|p| rust_type_name(&p));
            match qualifier.as_deref() {
                Some("Self") => Some(match &scope.self_type {
                    Some(t) => format!("{t}::{name}"),
                    None => name,
                }),
                Some(q) if q.chars().next().is_some_and(|c| c.is_uppercase()) => {
                    Some(format!("{q}::{name}"))
                }
                _ => Some(name),
            }
        }
        "field_expression" => {
            let field = field_text(ctx, f, "field")?;
            let on_self = f.child_by_field_name("value").is_some_and(|v| v.kind() == "self");
            Some(match (&scope.self_type, on_self) {
                (Some(t), true) => format!("{t}::{field}"),
                _ => field,
            })
        }
        "generic_function" => f
            .child_by_field_name("function")
            .and_then(|g| rust_callee(ctx, g, scope)),
        _ => None,
    }
}

fn rust_type_name(raw: &str) -> String {
    let base = raw.split('<').next().unwrap_or(raw);
    let base = base.trim().trim_start_matches('&').trim_start_matches("mut ").trim();
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

fn visit_rust(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "function_item" | "function_signature_item" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let kind = if scope.in_owner { UnitKind::Method } else { UnitKind::Function };
            let label = label_for(scope, &name);
            let id = add_named(ctx, node, scope, &name, kind, label);
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: false, self_type: scope.self_type.clone() };
            visit_children(ctx, node, &inner);
        }
        "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item" | "macro_definition" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let kind = if node.kind() == "macro_definition" { UnitKind::Function } else { UnitKind::Type };
            let id = add_named(ctx, node, scope, &name, kind, name.clone());
            let is_trait = node.kind() == "trait_item";
            let inner = Scope {
                unit: id.clone(),
                prefix: id,
                in_owner: is_trait,
                self_type: if is_trait { Some(name.clone()) } else { scope.self_type.clone() },
            };
            visit_children(ctx, node, &inner);
        }
        "impl_item" => {
            let ty = field_text(ctx, node, "type").map(|t| rust_type_name(&t)).unwrap_or_default();
            let tr = field_text(ctx, node, "trait").map(|t| rust_type_name(&t));
            let label = match &tr {
                Some(t) => format!("impl {t} for {ty}"),
                None => format!("impl {ty}"),
            };
            let s = extended_start(ctx, node);
            let id = ctx.tree.add(
                &scope.unit,
                format!("{}::{}", scope.prefix, label),
                UnitKind::Impl,
                s,
                end_line(node),
                label,
            );
            if let Some(t) = &tr {
                ctx.tree.mention(&id, t, MentionKind::Type, start_line(node));
            }
            let inner = Scope {
                unit: id,
                prefix: format!("{}::{}", scope.prefix, ty),
                in_owner: true,
                self_type: Some(ty.clone()),
            };
            if let Some(body) = node.child_by_field_name("body") {
                visit_children(ctx, body, &inner);
            }
            if let Some(t) = node.child_by_field_name("type") {
                visit(ctx, t, &inner);
            }
        }
        "mod_item" => {
            let Some(name) = field_text(ctx, node, "name") else { return };
            if node.child_by_field_name("body").is_none() {
                ctx.tree.mention(&scope.unit, &name, MentionKind::Import, start_line(node));
                return;
            }
            let id = add_named(ctx, node, scope, &name, UnitKind::Module, format!("mod {name}"));
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: false, self_type: scope.self_type.clone() };
            visit_children(ctx, node, &inner);
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                if let Some(name) = rust_callee(ctx, f, scope) {
                    ctx.tree.mention(&scope.unit, &name, MentionKind::Call, start_line(node));
                }
            }
            visit_children(ctx, node, scope);
        }
        "type_identifier" => {
            if !is_def_name(node) {
                let name = text(ctx, node).to_string();
                ctx.tree.mention(&scope.unit, &name, MentionKind::Type, start_line(node));
            }
        }
        "use_declaration" => mention_identifiers(ctx, node, scope, MentionKind::Import, false),
        "line_comment" | "block_comment" | "string_literal" | "raw_string_literal" => {}
        _ => visit_children(ctx, node, scope),
    }
}

fn label_for(scope: &Scope, name: &str) -> String {
    if scope.in_owner {
        let owner = scope.prefix.rsplit("::").next().unwrap_or("");
        format!("{owner}::{name}")
    } else {
        name.to_string()
    }
}

// -------------------------------------------------------------------- Python

fn visit_python(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "function_definition" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let kind = if scope.in_owner { UnitKind::Method } else { UnitKind::Function };
            let label = label_for(scope, &name);
            let id = add_named(ctx, node, scope, &name, kind, label);
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: false, self_type: scope.self_type.clone() };
            visit_children(ctx, node, &inner);
        }
        "class_definition" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let id = add_named(ctx, node, scope, &name, UnitKind::Type, name.clone());
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: true, self_type: Some(name.clone()) };
            if let Some(bases) = node.child_by_field_name("superclasses") {
                mention_identifiers(ctx, bases, &inner, MentionKind::Type, false);
            }
            if let Some(body) = node.child_by_field_name("body") {
                visit_children(ctx, body, &inner);
            }
        }
        "call" => {
            if let Some(f) = node.child_by_field_name("function") {
                let name = match f.kind() {
                    "identifier" => Some(text(ctx, f).to_string()),
                    "attribute" => field_text(ctx, f, "attribute").map(|a| {
                        let on_self = f
                            .child_by_field_name("object")
                            .is_some_and(|o| text(ctx, o) == "self" || text(ctx, o) == "cls");
                        match (&scope.self_type, on_self) {
                            (Some(t), true) => format!("{t}::{a}"),
                            _ => a,
                        }
                    }),
                    _ => None,
                };
                if let Some(name) = name {
                    ctx.tree.mention(&scope.unit, &name, MentionKind::Call, start_line(node));
                }
            }
            visit_children(ctx, node, scope);
        }
        "import_statement" | "import_from_statement" => {
            mention_identifiers(ctx, node, scope, MentionKind::Import, false)
        }
        "type" => mention_identifiers(ctx, node, scope, MentionKind::Type, false),
        "comment" | "string" => {}
        _ => visit_children(ctx, node, scope),
    }
}

// ---------------------------------------------------------------------- Java

fn visit_java(ctx: &mut Ctx<'_, '_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
        | "annotation_type_declaration" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let id = add_named(ctx, node, scope, &name, UnitKind::Type, name.clone());
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: true, self_type: Some(name.clone()) };
            visit_children(ctx, node, &inner);
        }
        "method_declaration" | "constructor_declaration" => {
            let Some(name) = field_text(ctx, node, "name") else {
                return visit_children(ctx, node, scope);
            };
            let label = label_for(scope, &name);
            let id = add_named(ctx, node, scope, &name, UnitKind::Method, label);
            let inner = Scope { unit: id.clone(), prefix: id, in_owner: false, self_type: scope.self_type.clone() };
            visit_children(ctx, node, &inner);
        }
        "method_invocation" => {
            if let Some(name) = field_text(ctx, node, "name") {
                ctx.tree.mention(&scope.unit, &name, MentionKind::Call, start_line(node));
            }
            visit_children(ctx, node, scope);
        }
        "type_identifier" => {
            if !is_def_name(node) {
                let name = text(ctx, node).to_string();
                ctx.tree.mention(&scope.unit, &name, MentionKind::Type, start_line(node));
            }
        }
        "import_declaration" => mention_identifiers(ctx, node, scope, MentionKind::Import, true),
        "line_comment" | "block_comment" | "string_literal" => {}
        _ => visit_children(ctx, node, scope),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ex: &Extracted) -> Vec<&str> {
        ex.units.iter().map(|u| u.id.as_str()).collect()
    }

    #[test]
    fn rust_units_defs_mentions() {
        let src = r#"use crate::store::Store;

/// Open things.
#[inline]
pub fn open() -> Store {
    Store::new()
}

pub struct Index { n: usize }

impl Index {
    pub fn search(&self) -> usize { helper(self.n) }
}

impl Default for Index {
    fn default() -> Self { Index { n: 0 } }
}

fn helper(n: usize) -> usize { n }
"#;
        let ex = extract("src/a.rs", src, Lang::Rust).unwrap();
        assert_eq!(
            ids(&ex),
            vec![
                "src/a.rs",
                "src/a.rs/~1",
                "src/a.rs::open",
                "src/a.rs::Index",
                "src/a.rs::impl Index",
                "src/a.rs::Index::search",
                "src/a.rs::impl Default for Index",
                "src/a.rs::Index::default",
                "src/a.rs::helper",
            ]
        );
        let open = ex.units.iter().find(|u| u.id == "src/a.rs::open").unwrap();
        assert_eq!(open.start_line, 3, "doc comment and attribute attach to the fn");
        let search = ex.units.iter().find(|u| u.id == "src/a.rs::Index::search").unwrap();
        assert_eq!(search.kind, UnitKind::Method);
        assert_eq!(search.label, "Index::search");

        let calls: Vec<_> = ex
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Call)
            .map(|m| (m.unit_id.as_str(), m.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![("src/a.rs::open", "Store::new"), ("src/a.rs::Index::search", "helper")]
        );
        assert!(ex
            .mentions
            .iter()
            .any(|m| m.kind == MentionKind::Import && m.name == "Store"));
        assert!(ex
            .mentions
            .iter()
            .any(|m| m.kind == MentionKind::Type && m.name == "Default"));
        let defs: Vec<_> = ex.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            defs,
            vec!["open", "Index", "search", "Index::search", "default", "Index::default", "helper"]
        );
    }

    #[test]
    fn python_classes_methods_calls() {
        let src = "import os\nfrom pkg.mod import Thing\n\n@decorator\ndef top(x: Thing):\n    return helper(x)\n\nclass Model(Base):\n    def fit(self):\n        self.step()\n\n    def step(self):\n        pass\n";
        let ex = extract("m.py", src, Lang::Python).unwrap();
        assert_eq!(
            ids(&ex),
            vec!["m.py", "m.py/~1", "m.py::top", "m.py::Model", "m.py::Model::fit", "m.py::Model::step"]
        );
        let top = ex.units.iter().find(|u| u.id == "m.py::top").unwrap();
        assert_eq!(top.start_line, 4, "decorator belongs to the function");
        let names: Vec<_> = ex.mentions.iter().map(|m| (m.kind, m.name.as_str())).collect();
        assert!(names.contains(&(MentionKind::Call, "helper")));
        assert!(names.contains(&(MentionKind::Call, "Model::step")));
        assert!(names.contains(&(MentionKind::Type, "Base")));
        assert!(names.contains(&(MentionKind::Type, "Thing")));
        assert!(names.contains(&(MentionKind::Import, "Thing")));
    }

    #[test]
    fn java_types_and_invocations() {
        let src = "import java.util.List;\n\npublic class Repo {\n  private List<Item> items;\n  public Repo() {}\n  public Item find(String id) { return lookup(id); }\n}\n";
        let ex = extract("Repo.java", src, Lang::Java).unwrap();
        assert_eq!(
            ids(&ex),
            vec!["Repo.java", "Repo.java/~1", "Repo.java::Repo", "Repo.java::Repo/~1", "Repo.java::Repo::Repo", "Repo.java::Repo::find"]
        );
        let names: Vec<_> = ex.mentions.iter().map(|m| (m.kind, m.name.as_str())).collect();
        assert!(names.contains(&(MentionKind::Import, "List")));
        assert!(names.contains(&(MentionKind::Call, "lookup")));
        assert!(names.contains(&(MentionKind::Type, "Item")));
    }
}
