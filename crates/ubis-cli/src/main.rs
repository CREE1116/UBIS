//! `ubis` — where, not what.
//!
//! A deterministic local index that hands an agent a short list of exact
//! spans instead of a map to explore.

mod session;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use session::{Session, DB_DIR, DB_FILE};

#[derive(Parser)]
#[command(name = "ubis", version, about = "Unit-level local index for agents: where, not what")]
struct Cli {
    /// Index database. Default: nearest `.ubis/index.db` above the current directory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Emit JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// Answer from the index as is, without first re-indexing changed files.
    #[arg(long, global = true)]
    no_refresh: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a directory (incremental). In a git repository, also records
    /// history and derives co-change.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Skip git history (no co-change).
        #[arg(long)]
        no_git: bool,
        /// Accepted for compatibility; git history is on by default.
        #[arg(long, hide = true)]
        git: bool,
    },
    /// Index, then keep the index current from filesystem events.
    Watch {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Quiet period before a batch of events is applied.
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },
    /// Find candidate units for a description, optionally from an anchor unit.
    Find {
        /// Query text (may be empty when --anchor is given).
        text: Vec<String>,
        /// Unit the agent is looking at: ID, name, or `path:line`.
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
        /// ID, name, or `path:line`.
        unit: String,
        #[arg(long)]
        out: bool,
    },
    /// Units near an anchor: references in and out, siblings, co-change.
    Near {
        /// ID, name, or `path:line`.
        unit: String,
        #[arg(short, long, default_value_t = 10)]
        k: usize,
    },
    /// Every recorded reference to a unit (exact, with resolution mass).
    Refs {
        /// ID, name, or `path:line`.
        unit: String,
    },
    /// Index statistics.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Index { path, no_git, .. } => cmd_index(&cli, path, !no_git),
        Cmd::Watch { path, debounce_ms } => cmd_watch(&cli, path, *debounce_ms),
        Cmd::Find { text, anchor, scope, k } => {
            let s = open(&cli)?;
            let hits = s.find(&text.join(" "), anchor.as_deref(), scope.as_deref(), *k)?;
            print_hits(&cli, &hits)
        }
        Cmd::Near { unit, k } => {
            let s = open(&cli)?;
            print_hits(&cli, &s.near(unit, *k)?)
        }
        Cmd::Open { unit, out } => {
            let s = open(&cli)?;
            let (u, children) = s.open(unit, *out)?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct Out<'a> {
                    unit: &'a ubis_core::UnitRow,
                    children: Vec<(&'a str, usize, usize, &'a str)>,
                }
                let children = children
                    .iter()
                    .map(|c| (c.id.as_str(), c.start_line, c.end_line, c.label.as_str()))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&Out { unit: &u, children })?);
            } else {
                print!("{}", Session::render_open(&u, &children));
            }
            Ok(())
        }
        Cmd::Refs { unit } => {
            let s = open(&cli)?;
            let (u, edges) = s.refs(unit)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            } else {
                print!("{}", s.render_refs(&u, &edges)?);
            }
            Ok(())
        }
        Cmd::Status => {
            let s = open(&cli)?;
            let st = s.store.stats()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "root {}\nfiles {}  units {} (leaves {})  definitions {}  mentions {}  edges {}",
                    s.root.display(),
                    st.files,
                    st.units,
                    st.leaves,
                    st.definitions,
                    st.mentions,
                    st.edges
                );
            }
            Ok(())
        }
    }
}

/// Open the index and bring it up to date (unless `--no-refresh`).
fn open(cli: &Cli) -> Result<Session> {
    let mut s = Session::locate(cli.db.as_deref())?;
    if !cli.no_refresh {
        let r = s.refresh()?;
        if r.changed() {
            eprintln!("ubis: refreshed {r}");
        }
    }
    Ok(s)
}

fn print_hits(cli: &Cli, hits: &[ubis_core::Hit]) -> Result<()> {
    if cli.json {
        println!("{}", serde_json::to_string_pretty(hits)?);
    } else {
        print!("{}", Session::render_hits(hits));
    }
    Ok(())
}

fn root_and_db(cli: &Cli, path: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let db = cli.db.clone().unwrap_or_else(|| root.join(DB_DIR).join(DB_FILE));
    Ok((root, db))
}

fn cmd_index(cli: &Cli, path: &Path, git: bool) -> Result<()> {
    let (root, db) = root_and_db(cli, path)?;
    let mut s = Session::create(&root, &db)?;
    let r = s.index(git, true)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&r.files)?);
    } else {
        println!("indexed {} in {r}", root.display());
        println!("db: {}", db.display());
    }
    Ok(())
}

fn cmd_watch(cli: &Cli, path: &Path, debounce_ms: u64) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;
    use std::time::Duration;

    let (root, db) = root_and_db(cli, path)?;
    let mut s = Session::create(&root, &db)?;
    let r = s.index(true, true)?;
    println!("watching {} — initial: {r}", root.display());

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev.paths);
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    let db_dir = db.parent().map(Path::to_path_buf);

    loop {
        let first = rx.recv().context("watcher stopped")?;
        let mut paths = first;
        while let Ok(more) = rx.recv_timeout(Duration::from_millis(debounce_ms)) {
            paths.extend(more);
        }
        // Ignore our own writes; anything else (including `.git`, so commits
        // and checkouts re-derive history) triggers a refresh. Unchanged
        // files are skipped by stat, so this stays cheap.
        if paths.iter().all(|p| db_dir.as_ref().is_some_and(|d| p.starts_with(d))) {
            continue;
        }
        match s.refresh() {
            Ok(r) if r.changed() => println!("{r}"),
            Ok(_) => {}
            Err(e) => eprintln!("update failed: {e:#}"),
        }
    }
}
