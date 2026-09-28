//! `ubis` — where, not what.
//!
//! A deterministic local index that hands an agent a short list of exact
//! spans instead of a map to explore.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ubis_core::query::{search, Hit, Query};
use ubis_core::{Store, UnitRow};

const DB_DIR: &str = ".ubis";
const DB_FILE: &str = "index.db";

#[derive(Parser)]
#[command(name = "ubis", version, about = "Unit-level local index for agents: where, not what")]
struct Cli {
    /// Index database. Default: nearest `.ubis/index.db` above the current directory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Emit JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a directory (incremental: only changed files are re-extracted).
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also record git commits and hunks (evidence for co-change and evaluation).
        #[arg(long)]
        git: bool,
    },
    /// Find candidate units for a description, optionally from an anchor unit.
    Find {
        /// Query text (may be empty when --anchor is given).
        text: Vec<String>,
        /// Unit the agent is currently looking at.
        #[arg(short, long)]
        anchor: Option<String>,
        /// Restrict to paths with this prefix.
        #[arg(short, long)]
        scope: Option<String>,
        /// Maximum number of results; the cut is adaptive below this.
        #[arg(short, long, default_value_t = 10)]
        k: usize,
    },
    /// Print a unit's span. `--out` zooms to the parent; containers list children.
    Open {
        unit: String,
        #[arg(long)]
        out: bool,
    },
    /// Units near an anchor: references in and out, and siblings.
    Near {
        unit: String,
        #[arg(short, long, default_value_t = 10)]
        k: usize,
    },
    /// Every recorded reference to a unit (exact, with resolution mass).
    Refs { unit: String },
    /// Index statistics.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Index { path, git } => cmd_index(&cli, path, *git),
        Cmd::Find { text, anchor, scope, k } => {
            let store = open_store(&cli)?;
            let q = Query {
                text: text.join(" "),
                anchor: anchor.clone(),
                scope: scope.clone(),
                k_max: *k,
            };
            if q.text.trim().is_empty() && q.anchor.is_none() {
                bail!("give query text, --anchor, or both");
            }
            print_hits(&cli, &search(&store, &q)?.hits)
        }
        Cmd::Near { unit, k } => {
            let store = open_store(&cli)?;
            let u = resolve_unit(&store, unit)?;
            let q = Query {
                text: String::new(),
                anchor: Some(u.id),
                scope: None,
                k_max: *k,
            };
            print_hits(&cli, &search(&store, &q)?.hits)
        }
        Cmd::Open { unit, out } => {
            let store = open_store(&cli)?;
            let mut u = resolve_unit(&store, unit)?;
            if *out {
                if let Some(p) = u.parent.clone() {
                    u = store.unit(&p)?.context("parent missing")?;
                }
            }
            cmd_open(&cli, &store, &u)
        }
        Cmd::Refs { unit } => {
            let store = open_store(&cli)?;
            let u = resolve_unit(&store, unit)?;
            let mut edges = Vec::new();
            for s in store.subtree(&u.id)? {
                edges.extend(store.edges_to(&s.id)?);
            }
            edges.sort_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.src.cmp(&b.src)));
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            } else if edges.is_empty() {
                println!("no recorded references to {}", u.id);
            } else {
                for e in edges {
                    let src = store.unit(&e.src)?;
                    let loc = src
                        .map(|s| format!("{}:{}", s.path, e.line))
                        .unwrap_or_default();
                    println!(
                        "{:<6} {:<9} w={:.2}  {}  ({})  → {}",
                        e.kind.as_str(),
                        e.origin,
                        e.weight,
                        e.src,
                        loc,
                        e.dst
                    );
                }
            }
            Ok(())
        }
        Cmd::Status => {
            let store = open_store(&cli)?;
            let s = store.stats()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!(
                    "files {}  units {} (leaves {})  definitions {}  mentions {}  edges {}",
                    s.files, s.units, s.leaves, s.definitions, s.mentions, s.edges
                );
            }
            Ok(())
        }
    }
}

fn cmd_index(cli: &Cli, path: &Path, git: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let db = cli.db.clone().unwrap_or_else(|| root.join(DB_DIR).join(DB_FILE));
    let mut store = Store::open(&db)?;
    let started = std::time::Instant::now();
    let report = ubis_ingest::index_dir(&mut store, &root)?;
    let mut commits = 0;
    if git && ubis_git::is_repo(&root) {
        let history = ubis_git::history(&root, "HEAD", 5000)?;
        commits = history.len();
        let rows: Vec<_> = history
            .into_iter()
            .map(|c| {
                let hunks = c.hunks.into_iter().map(|h| (h.path, h.start, h.len)).collect();
                (c.id, c.ts, c.subject, hunks)
            })
            .collect();
        store.replace_history(&rows)?;
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "indexed {} in {:.2}s — added {}, updated {}, unchanged {}, removed {}, skipped {}; edges {}{}",
            root.display(),
            started.elapsed().as_secs_f64(),
            report.added,
            report.updated,
            report.unchanged,
            report.removed,
            report.skipped,
            report.edges,
            if git { format!("; commits {commits}") } else { String::new() }
        );
        println!("db: {}", db.display());
    }
    Ok(())
}

fn open_store(cli: &Cli) -> Result<Store> {
    let db = match &cli.db {
        Some(p) => p.clone(),
        None => find_db(&std::env::current_dir()?)
            .context("no .ubis/index.db found here or above; run `ubis index` first")?,
    };
    Store::open(&db)
}

fn find_db(start: &Path) -> Option<PathBuf> {
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

fn resolve_unit(store: &Store, reference: &str) -> Result<UnitRow> {
    let matches = store.find_unit(reference)?;
    match matches.len() {
        0 => bail!("no unit matches `{reference}`"),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let exact: Vec<_> = matches.iter().filter(|u| u.id == reference).collect();
            if let Some(u) = exact.first() {
                return Ok((*u).clone());
            }
            let list: Vec<_> = matches.iter().take(10).map(|u| u.id.as_str()).collect();
            bail!("`{reference}` is ambiguous:\n  {}", list.join("\n  "))
        }
    }
}

fn print_hits(cli: &Cli, hits: &[Hit]) -> Result<()> {
    if cli.json {
        println!("{}", serde_json::to_string_pretty(hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no candidates");
    }
    for (i, h) in hits.iter().enumerate() {
        let lifted = if h.lifted > 0 {
            format!("  [{} matches inside]", h.lifted)
        } else {
            String::new()
        };
        println!(
            "[{}] {}:{}-{}  {}  ({}){}",
            i + 1,
            h.path,
            h.start_line,
            h.end_line,
            h.id,
            h.kind.as_str(),
            lifted
        );
        if !h.signature.is_empty() {
            println!("    {}", h.signature);
        }
        let via: Vec<_> = h
            .via
            .iter()
            .map(|v| format!("{} {:.2}", v.op, v.contribution))
            .collect();
        println!("    via {}", via.join(", "));
    }
    Ok(())
}

fn cmd_open(cli: &Cli, store: &Store, u: &UnitRow) -> Result<()> {
    let children = store.children(&u.id)?;
    if cli.json {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            unit: &'a UnitRow,
            children: Vec<(&'a str, usize, usize, &'a str)>,
        }
        let out = Out {
            unit: u,
            children: children
                .iter()
                .map(|c| (c.id.as_str(), c.start_line, c.end_line, c.label.as_str()))
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("{}:{}-{}  {}  ({})", u.path, u.start_line, u.end_line, u.id, u.kind.as_str());
    if let Some(p) = &u.parent {
        println!("parent: {p}");
    }
    if children.is_empty() {
        println!("---");
        for (i, line) in u.text.lines().enumerate() {
            println!("{:>5}  {}", u.start_line + i, line);
        }
    } else {
        println!("children:");
        for c in children {
            println!("  {:>5}-{:<5} {}  ({})", c.start_line, c.end_line, c.id, c.kind.as_str());
        }
    }
    Ok(())
}
