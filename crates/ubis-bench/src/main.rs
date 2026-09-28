//! `ubis-bench` — evaluation harness with a temporal split.
//!
//! Git history is both evidence and judge, so it is split in time:
//!
//! ```text
//!   commits ≤ T0            │  commits > T0
//!   index the tree at T0    │  each commit becomes queries; the units it
//!   (evidence)              │  touched are the answer (never seen by the index)
//! ```
//!
//! Query modes per held-out commit with touched units `G` (as they exist at T0):
//!
//! * `text`:        commit subject → find `G`
//! * `anchor`:      first unit of `G` as anchor, no text → find the rest
//! * `anchor+text`: both
//!
//! Baseline `grep-read` mimics an agent without an index: rank files by how
//! many query terms they contain, then read the top files whole (`text`), or
//! read the anchor's whole file (`anchor`).
//!
//! Metrics: Recall@K (a hit covers a gold unit if it is that unit or one of
//! its ancestors), hit rate (any gold found), and tokens the agent would read.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Parser;
use serde::Serialize;
use ubis_core::query::{plan, search_with, Query};
use ubis_core::tokenize::tokenize;
use ubis_core::Store;

#[derive(Parser)]
#[command(name = "ubis-bench", about = "Temporal-split retrieval evaluation from git history")]
struct Args {
    /// Git repository to evaluate on.
    repo: PathBuf,
    /// Number of most recent commits held out as queries.
    #[arg(long, default_value_t = 50)]
    holdout: usize,
    /// Result budget per query.
    #[arg(short, long, default_value_t = 10)]
    k: usize,
    /// Skip commits touching more units than this (bulk edits).
    #[arg(long, default_value_t = 40)]
    max_gold: usize,
    /// Operators to disable (comma-separated), for ablations.
    #[arg(long, value_delimiter = ',')]
    disable: Vec<String>,
    /// Override operator weights, e.g. `--weight lexical=0.5,tree_near=0.2`.
    #[arg(long, value_delimiter = ',')]
    weight: Vec<String>,
    /// Maximum commits read from history.
    #[arg(long, default_value_t = 5000)]
    max_commits: usize,
    #[arg(long)]
    json: bool,
    /// Print each query and its outcome.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Default, Serialize, Clone)]
struct Agg {
    queries: usize,
    recall_sum: f64,
    hits: usize,
    returned_sum: usize,
    read_tokens_sum: f64,
}

impl Agg {
    fn add(&mut self, recall: f64, returned: usize, tokens: f64) {
        self.queries += 1;
        self.recall_sum += recall;
        if recall > 0.0 {
            self.hits += 1;
        }
        self.returned_sum += returned;
        self.read_tokens_sum += tokens;
    }
    fn row(&self, name: &str) -> String {
        let n = self.queries.max(1) as f64;
        format!(
            "{:<22} {:>5} {:>9.3} {:>9.3} {:>9.1} {:>11.0}",
            name,
            self.queries,
            self.recall_sum / n,
            self.hits as f64 / n,
            self.returned_sum as f64 / n,
            self.read_tokens_sum / n
        )
    }
}

fn tokens_of(text: &str) -> f64 {
    // A rough, method-independent proxy: ~4 bytes per token.
    (text.len() as f64 / 4.0).ceil()
}

fn main() -> Result<()> {
    let args = Args::parse();
    let repo = args.repo.canonicalize()?;
    if !ubis_git::is_repo(&repo) {
        bail!("{} is not a git repository", repo.display());
    }
    let commits = ubis_git::history(&repo, "HEAD", args.max_commits)?;
    if commits.len() < 4 {
        bail!("need at least 4 commits, found {}", commits.len());
    }
    let holdout = args.holdout.min(commits.len() / 2).max(1);
    let split = commits.len() - holdout;
    let t0 = &commits[split - 1];
    let held = &commits[split..];

    // Evidence: the tree at T0.
    let tmp = tempfile::tempdir()?;
    ubis_git::export_tree(&repo, &t0.id, tmp.path())?;
    let mut store = Store::open(&tmp.path().join(".ubis-bench.db"))?;
    let report = ubis_ingest::index_dir(&mut store, tmp.path())?;
    let stats = store.stats()?;
    eprintln!(
        "T0 = {} ({} commits of evidence, {} held out); index: {} files, {} leaves, {} edges",
        &t0.id[..10],
        split,
        held.len(),
        report.added,
        stats.leaves,
        stats.edges
    );

    let file_texts = load_files(&store)?;
    let mut aggs: BTreeMap<&'static str, Agg> = BTreeMap::new();
    let mut skipped_empty = 0;
    let mut skipped_bulk = 0;

    for c in held {
        let gold = touched_units(&repo, c, &store)?;
        if gold.is_empty() {
            skipped_empty += 1;
            continue;
        }
        if gold.len() > args.max_gold {
            skipped_bulk += 1;
            continue;
        }
        let gold_vec: Vec<String> = gold.iter().cloned().collect();

        // text
        if !tokenize(&c.subject).is_empty() {
            let q = Query {
                text: c.subject.clone(),
                anchor: None,
                scope: None,
                k_max: args.k,
            };
            let (r, n, t) = run(&store, &q, &gold, &args)?;
            aggs.entry("ubis text").or_default().add(r, n, t);
            if args.verbose {
                eprintln!("[text] {:.2} {:>3} | {} | gold {:?}", r, n, c.subject, gold_vec);
            }
            for files in [1usize, 3] {
                let (r, t) = grep_read(&c.subject, files, &gold, &file_texts, &store)?;
                let name = if files == 1 { "grep-read@1 text" } else { "grep-read@3 text" };
                aggs.entry(name).or_default().add(r, files, t);
            }
        }

        // anchor, anchor+text
        if gold_vec.len() >= 2 {
            let anchor = gold_vec[0].clone();
            let rest: BTreeSet<String> = gold_vec[1..].iter().cloned().collect();
            for (name, text) in [("ubis anchor", String::new()), ("ubis anchor+text", c.subject.clone())] {
                let q = Query {
                    text,
                    anchor: Some(anchor.clone()),
                    scope: None,
                    k_max: args.k,
                };
                let (r, n, t) = run(&store, &q, &rest, &args)?;
                aggs.entry(name).or_default().add(r, n, t);
                if args.verbose {
                    eprintln!("[{name}] {:.2} {:>3} | {} | anchor {}", r, n, c.subject, anchor);
                }
            }
            // Baseline: open the anchor's whole file.
            let path = store.unit(&anchor)?.map(|u| u.path).unwrap_or_default();
            let covered = rest
                .iter()
                .filter(|g| store.unit(g).ok().flatten().is_some_and(|u| u.path == path))
                .count();
            let t = file_texts.get(&path).map(|s| tokens_of(s)).unwrap_or(0.0);
            aggs.entry("read-anchor-file")
                .or_default()
                .add(covered as f64 / rest.len() as f64, 1, t);
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&aggs)?);
        return Ok(());
    }
    println!(
        "{:<22} {:>5} {:>9} {:>9} {:>9} {:>11}",
        "method", "n", "recall", "hit", "returned", "read_tok"
    );
    for (name, a) in &aggs {
        println!("{}", a.row(name));
    }
    println!(
        "skipped: {skipped_empty} commits touch no unit present at T0, {skipped_bulk} bulk commits (> {} units)",
        args.max_gold
    );
    Ok(())
}

/// Leaf units touched by a commit, mapped onto units that exist at T0.
///
/// Named units (`path::Type::method`, `path#section`) match by ID. Ordinal
/// units (`¶3`, `~2`, `code1`) shift when text is inserted above them, so they
/// match the T0 leaf in the same file with the highest token Jaccard (≥ 0.3).
fn touched_units(repo: &Path, c: &ubis_git::Commit, store: &Store) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for path in c.files() {
        let Some(content) = ubis_git::show(repo, &c.id, path) else { continue };
        let Some(content) = ubis_ingest::admit(Path::new(path), content.as_bytes()) else {
            continue;
        };
        let Ok(ex) = ubis_ingest::extract(path, &content) else { continue };
        let t0_leaves: Vec<(String, BTreeSet<String>)> = store
            .units_in_file(path)?
            .into_iter()
            .filter(|u| u.is_leaf)
            .map(|u| (u.id, tokenize(&u.text).into_iter().collect()))
            .collect();
        let parents: BTreeSet<&str> = ex.units.iter().filter_map(|u| u.parent.as_deref()).collect();
        for h in c.hunks.iter().filter(|h| h.path == path) {
            let (a, b) = (h.start, h.start + h.len - 1);
            for u in &ex.units {
                if parents.contains(u.id.as_str()) || !(u.start_line <= b && a <= u.end_line) {
                    continue;
                }
                if !is_ordinal(&u.id) {
                    if store.unit(&u.id)?.is_some() {
                        out.insert(u.id.clone());
                    }
                    continue;
                }
                let toks: BTreeSet<String> = tokenize(&u.text).into_iter().collect();
                let best = t0_leaves
                    .iter()
                    .filter(|(id, _)| is_ordinal(id))
                    .map(|(id, t)| (jaccard(&toks, t), id))
                    .filter(|(j, _)| *j >= 0.3)
                    .max_by(|x, y| x.0.total_cmp(&y.0).then_with(|| y.1.cmp(x.1)));
                if let Some((_, id)) = best {
                    out.insert(id.clone());
                }
            }
        }
    }
    Ok(out)
}

fn is_ordinal(id: &str) -> bool {
    let last = id.rsplit(['/', '#']).next().unwrap_or("");
    last.starts_with('¶') || last.starts_with('~') || last.starts_with("code") || !id.contains(['/', '#', ':'])
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len() as f64 + b.len() as f64 - inter)
}

/// Run one query; returns (recall, number returned, tokens to read the spans).
fn run(store: &Store, q: &Query, gold: &BTreeSet<String>, args: &Args) -> Result<(f64, usize, f64)> {
    let mut p = plan(q);
    p.ops.retain(|(op, _)| !args.disable.iter().any(|d| d == op.name()));
    for spec in &args.weight {
        if let Some((name, w)) = spec.split_once('=') {
            if let Ok(w) = w.parse::<f64>() {
                for (op, weight) in p.ops.iter_mut() {
                    if op.name() == name {
                        *weight = w;
                    }
                }
            }
        }
    }
    let resp = search_with(store, q, p)?;
    let hit_ids: BTreeSet<&str> = resp.hits.iter().map(|h| h.id.as_str()).collect();
    let mut found = 0;
    for g in gold {
        let covered = hit_ids.contains(g.as_str())
            || store.ancestors(g)?.iter().any(|a| hit_ids.contains(a.id.as_str()));
        if covered {
            found += 1;
        }
    }
    let mut tokens = 0.0;
    for h in &resp.hits {
        for u in store.subtree(&h.id)? {
            if u.is_leaf {
                tokens += tokens_of(&u.text);
            }
        }
    }
    Ok((found as f64 / gold.len() as f64, resp.hits.len(), tokens))
}

/// Full text of each indexed file, reassembled from its leaves.
fn load_files(store: &Store) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for path in store.file_paths()? {
        let text: Vec<String> = store
            .units_in_file(&path)?
            .into_iter()
            .filter(|u| u.is_leaf)
            .map(|u| u.text)
            .collect();
        out.insert(path, text.join("\n"));
    }
    Ok(out)
}

/// Baseline: rank files by distinct query terms present (ties: total
/// occurrences, then path), read the top `files` files whole.
fn grep_read(
    subject: &str,
    files: usize,
    gold: &BTreeSet<String>,
    texts: &HashMap<String, String>,
    store: &Store,
) -> Result<(f64, f64)> {
    let mut terms = tokenize(subject);
    terms.sort();
    terms.dedup();
    let mut ranked: Vec<(usize, usize, &String)> = texts
        .iter()
        .map(|(path, text)| {
            let toks = tokenize(text);
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for t in &toks {
                *counts.entry(t.as_str()).or_default() += 1;
            }
            let distinct = terms.iter().filter(|t| counts.contains_key(t.as_str())).count();
            let total: usize = terms.iter().map(|t| counts.get(t.as_str()).copied().unwrap_or(0)).sum();
            (distinct, total, path)
        })
        .filter(|(d, _, _)| *d > 0)
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    let chosen: BTreeSet<&str> = ranked.iter().take(files).map(|r| r.2.as_str()).collect();
    let mut found = 0;
    for g in gold {
        if store.unit(g)?.is_some_and(|u| chosen.contains(u.path.as_str())) {
            found += 1;
        }
    }
    let tokens: f64 = chosen.iter().map(|p| tokens_of(&texts[*p])).sum();
    Ok((found as f64 / gold.len() as f64, tokens))
}
