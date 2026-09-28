//! Unit model shared by extractors, the evidence store, and the query layer.
//!
//! A *unit* is the smallest addressable piece of readable text: a function, a
//! Markdown section, a paragraph. Units form a tree per file. Only leaves are
//! indexed lexically; containers are reached through their children.

use serde::{Deserialize, Serialize};

/// Structural unit identifier, e.g. `src/lib.rs::Store::open` or
/// `docs/a.md#install/linux`. IDs are derived from structure, not line
/// numbers, so they survive edits that only move text.
pub type UnitId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    File,
    Module,
    Type,
    Impl,
    Function,
    Method,
    Section,
    Paragraph,
    CodeBlock,
    /// Lines of a container that no named child covers (imports, fields, prose).
    Gap,
}

impl UnitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UnitKind::File => "file",
            UnitKind::Module => "module",
            UnitKind::Type => "type",
            UnitKind::Impl => "impl",
            UnitKind::Function => "function",
            UnitKind::Method => "method",
            UnitKind::Section => "section",
            UnitKind::Paragraph => "paragraph",
            UnitKind::CodeBlock => "code_block",
            UnitKind::Gap => "gap",
        }
    }

    /// Units whose ID ends in a position (`¶3`, `~2`, `code1`) rather than a
    /// name, so the ID shifts when text is inserted above them.
    pub fn is_ordinal(self) -> bool {
        matches!(self, UnitKind::Paragraph | UnitKind::CodeBlock | UnitKind::Gap)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "file" => UnitKind::File,
            "module" => UnitKind::Module,
            "type" => UnitKind::Type,
            "impl" => UnitKind::Impl,
            "function" => UnitKind::Function,
            "method" => UnitKind::Method,
            "section" => UnitKind::Section,
            "paragraph" => UnitKind::Paragraph,
            "code_block" => UnitKind::CodeBlock,
            "gap" => UnitKind::Gap,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub id: UnitId,
    pub parent: Option<UnitId>,
    pub kind: UnitKind,
    pub path: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    /// 1-based, inclusive.
    pub end_line: usize,
    /// Short human label (symbol name or heading).
    pub label: String,
    /// Text of the unit. For containers this is only the header line; the
    /// body is represented by children.
    pub text: String,
}

impl Unit {
    /// First non-empty line, trimmed and bounded; used as a one-line preview.
    pub fn signature(&self) -> String {
        let line = self
            .text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let mut out: String = line.chars().take(160).collect();
        if line.chars().count() > 160 {
            out.push('…');
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefKind {
    /// A code symbol (function, type, method, module).
    Symbol,
    /// A document anchor. Name is `path#slug` or `path`.
    Anchor,
}

impl DefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DefKind::Symbol => "symbol",
            DefKind::Anchor => "anchor",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "symbol" => Some(DefKind::Symbol),
            "anchor" => Some(DefKind::Anchor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub unit_id: UnitId,
    /// Simple name used for resolution (`open`, not `Store::open`).
    pub name: String,
    pub kind: DefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    /// A call-shaped reference in code.
    Call,
    /// A method call on a receiver of unknown type (`x.len()`): it may just
    /// as well target a method outside the project.
    Method,
    /// A type reference in code.
    Type,
    /// An imported name.
    Import,
    /// An explicit document link. Name is the normalized target (`path#slug`).
    Link,
    /// An identifier written in prose that may name a code symbol.
    Bridge,
}

impl MentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MentionKind::Call => "call",
            MentionKind::Method => "method",
            MentionKind::Type => "type",
            MentionKind::Import => "import",
            MentionKind::Link => "link",
            MentionKind::Bridge => "bridge",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "call" => MentionKind::Call,
            "method" => MentionKind::Method,
            "type" => MentionKind::Type,
            "import" => MentionKind::Import,
            "link" => MentionKind::Link,
            "bridge" => MentionKind::Bridge,
            _ => return None,
        })
    }
}

/// An unresolved reference as written in the source. Edges are *derived*
/// from mentions and definitions, so a change to a definition never strands a
/// stale edge: resolution is simply recomputed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    pub unit_id: UnitId,
    pub name: String,
    pub kind: MentionKind,
    pub line: usize,
}

/// Output contract of every extractor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub path: String,
    pub units: Vec<Unit>,
    pub definitions: Vec<Definition>,
    pub mentions: Vec<Mention>,
}

/// A derived, weighted, directed relation between units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub src: UnitId,
    pub dst: UnitId,
    /// The mention kind that produced the edge.
    pub kind: MentionKind,
    /// How the target was chosen: `explicit`, `same_file`, or `global`.
    pub origin: String,
    /// Mass assigned to this target. An ambiguous mention with `m` candidate
    /// definitions contributes `1/m` to each.
    pub weight: f64,
    pub line: usize,
}
