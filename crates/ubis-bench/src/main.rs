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
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;
use serde::Serialize;
use ubis_core::cochange::CoChangeParams;
use ubis_core::query::{plan, search_with, Query};
use ubis_core::tokenize::tokenize_raw as tokenize;
use ubis_core::Store;
use ubis_ingest::history::History;

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
    /// Lower bound of the adaptive cut (set equal to -k to disable the cut).
    #[arg(long)]
    k_min: Option<usize>,
    /// Skip commits touching more units than this (bulk edits).
    #[arg(long, default_value_t = 40)]
    max_gold: usize,
    /// Operators to disable (comma-separated), for ablations.
    #[arg(long, value_delimiter = ',')]
    disable: Vec<String>,
    /// Override operator weights, e.g. `--weight lexical=0.5,tree_near=0.2`.
    #[arg(long, value_delimiter = ',')]
    weight: Vec<String>,
    /// Co-change decay time constant in days.
    #[arg(long, default_value_t = ubis_core::cochange::TAU_DAYS)]
    cochange_tau: f64,
    /// Minimum number of commits a co-change pair must appear in.
    #[arg(long, default_value_t = ubis_core::cochange::MIN_SUPPORT)]
    cochange_support: usize,
    /// Task file (JSONL: `{"id", "base", "head", "text"}`) instead of commit
    /// subjects: index the tree at `base`, ask `text`, answer = units changed
    /// by `base..head`. See `scripts/fetch_pr_tasks.py`.
    #[arg(long)]
    tasks: Option<PathBuf>,
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
    list_tokens_sum: f64,
    calls_sum: usize,
}

impl Agg {
    fn add(&mut self, recall: f64, returned: usize, tokens: f64) {
        self.add_cost(recall, returned, tokens, 0.0, 1);
    }

    /// `list` = tokens of the tool output an agent reads before opening any
    /// span; `calls` = tool calls spent.
    fn add_cost(&mut self, recall: f64, returned: usize, tokens: f64, list: f64, calls: usize) {
        self.list_tokens_sum += list;
        self.calls_sum += calls;
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
            "{:<22} {:>5} {:>9.3} {:>9.3} {:>9.1} {:>11.0} {:>9.0} {:>11.0} {:>6.1}",
            name,
            self.queries,
            self.recall_sum / n,
            self.hits as f64 / n,
            self.returned_sum as f64 / n,
            self.read_tokens_sum / n,
            self.list_tokens_sum / n,
            (self.read_tokens_sum + self.list_tokens_sum) / n,
            self.calls_sum as f64 / n
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
    if let Some(tasks) = &args.tasks {
        return run_tasks(&args, &repo, tasks);
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

    // Co-change from evidence commits only (≤ T0): no leakage into held-out.
    let started = std::time::Instant::now();
    let mut hist = History::open(&repo)?;
    let pairs = hist.record_and_derive(
        &mut store,
        &commits[..split],
        &CoChangeParams {
            tau: args.cochange_tau * 86400.0,
            min_support: args.cochange_support,
            max_set: args.max_gold,
            ..CoChangeParams::at(t0.ts)
        },
    )?;
    eprintln!("co-change: {pairs} pairs in {:.2}s", started.elapsed().as_secs_f64());

    let files = Files::load(&store)?;
    let mut aggs: BTreeMap<&'static str, Agg> = BTreeMap::new();
    let mut skipped_empty = 0;
    let mut skipped_bulk = 0;

    let mut mapper = hist.mapper(&store)?;
    for c in held {
        let gold = mapper.touched(c)?;
        if gold.is_empty() {
            skipped_empty += 1;
            continue;
        }
        if gold.len() > args.max_gold {
            skipped_bulk += 1;
            continue;
        }
        evaluate(&store, &files, &gold, &c.subject, &args, &mut aggs)?;
    }

    print_table(&args, &aggs)?;
    println!(
        "skipped: {skipped_empty} commits touch no unit present at T0, {skipped_bulk} bulk commits (> {} units)",
        args.max_gold
    );
    Ok(())
}

fn print_table(args: &Args, aggs: &BTreeMap<&'static str, Agg>) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(aggs)?);
        return Ok(());
    }
    println!(
        "{:<22} {:>5} {:>9} {:>9} {:>9} {:>11} {:>9} {:>11} {:>6}",
        "method", "n", "recall", "hit", "returned", "read_tok", "list_tok", "total_tok", "calls"
    );
    for (name, a) in aggs {
        println!("{}", a.row(name));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct Task {
    id: String,
    base: String,
    head: String,
    text: String,
}

/// Task mode: each task is evaluated against the tree at its own `base`
/// (one store, re-indexed incrementally), with co-change from history ≤ base.
fn run_tasks(args: &Args, repo: &std::path::Path, path: &std::path::Path) -> Result<()> {
    let tasks: Vec<Task> = std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let tmp = tempfile::tempdir()?;
    let tree = tmp.path().join("tree");
    let mut store = Store::open(&tmp.path().join("index.db"))?;
    let mut aggs: BTreeMap<&'static str, Agg> = BTreeMap::new();
    let (mut evaluated, mut skipped_empty, mut skipped_bulk) = (0, 0, 0);
    let started = std::time::Instant::now();
    // Parse history once; each task sees the `max_commits` commits up to its base.
    let all = ubis_git::history(repo, "HEAD", 100_000)?;
    let mut hist = History::open(repo)?;
    let pos: HashMap<&str, usize> = all.iter().enumerate().map(|(i, c)| (c.id.as_str(), i)).collect();
    for task in &tasks {
        if tree.exists() {
            std::fs::remove_dir_all(&tree)?;
        }
        ubis_git::export_tree(repo, &task.base, &tree)?;
        ubis_ingest::index_dir(&mut store, &tree)?;
        let owned;
        let history: &[ubis_git::Commit] = match pos.get(ubis_git::rev_parse(repo, &task.base)?.as_str()) {
            Some(&i) => &all[(i + 1).saturating_sub(args.max_commits)..=i],
            None => {
                owned = ubis_git::history(repo, &task.base, args.max_commits)?;
                &owned
            }
        };
        let now = history.last().map(|c| c.ts).unwrap_or(0);
        hist.record_and_derive(
            &mut store,
            history,
            &CoChangeParams {
                tau: args.cochange_tau * 86400.0,
                min_support: args.cochange_support,
                max_set: args.max_gold,
                ..CoChangeParams::at(now)
            },
        )?;
        let change = ubis_git::diff(repo, &task.base, &task.head)?;
        let gold = hist.mapper(&store)?.touched(&change)?;
        if gold.is_empty() {
            skipped_empty += 1;
            continue;
        }
        if gold.len() > args.max_gold {
            skipped_bulk += 1;
            continue;
        }
        if args.verbose {
            eprintln!("== {} ({} gold)", task.id, gold.len());
        }
        let files = Files::load(&store)?;
        evaluate(&store, &files, &gold, &task.text, args, &mut aggs)?;
        evaluated += 1;
    }
    eprintln!(
        "{} tasks: {evaluated} evaluated in {:.1}s",
        tasks.len(),
        started.elapsed().as_secs_f64()
    );
    print_table(args, &aggs)?;
    println!(
        "skipped: {skipped_empty} tasks touch no indexed unit, {skipped_bulk} bulk (> {} units)",
        args.max_gold
    );
    Ok(())
}

/// Indexed files reassembled for the grep baselines.
struct Files {
    texts: HashMap<String, String>,
    terms: HashMap<String, HashMap<String, usize>>,
}

impl Files {
    fn load(store: &Store) -> Result<Self> {
        let texts = load_files(store)?;
        let terms = term_counts(&texts);
        Ok(Self { texts, terms })
    }
}

/// Every method on one query/answer pair: `text` alone, the anchor modes
/// (first gold unit as anchor, the rest as answer), the two-call agent flow,
/// and the baselines.
fn evaluate(
    store: &Store,
    files: &Files,
    gold: &BTreeSet<String>,
    text: &str,
    args: &Args,
    aggs: &mut BTreeMap<&'static str, Agg>,
) -> Result<()> {
    let gold_vec: Vec<String> = gold.iter().cloned().collect();
    let query = |text: &str, anchor: Option<String>| Query {
        text: text.to_string(),
        anchor,
        scope: None,
        k_max: args.k,
        k_min: args.k_min,
    };
    let subject = text.lines().next().unwrap_or("");

    if !tokenize(text).is_empty() {
        let (r, n, t, hits, list) = run(store, &query(text, None), gold, args)?;
        aggs.entry("ubis text").or_default().add_cost(r, n, t, list, 1);
        if args.verbose {
            eprintln!("[text] {:.2} {:>3} | {} | gold {:?}", r, n, subject, gold_vec);
        }
        for k in [1usize, 3] {
            let (r, t) = grep_read(text, k, gold, files, store)?;
            let name = if k == 1 { "grep-read@1 text" } else { "grep-read@3 text" };
            aggs.entry(name).or_default().add(r, k, t);
        }
        // Agent flow: find, then `near` on the top hit with the same text; read both lists.
        if let Some(top) = hits.first() {
            let (_, _, _, more, list2) = run(store, &query(text, Some(top.clone())), gold, args)?;
            let mut seen: Vec<String> = hits.clone();
            for h in more {
                if !seen.contains(&h) {
                    seen.push(h);
                }
            }
            let (r, t) = score(store, &seen, gold)?;
            aggs.entry("ubis find->near").or_default().add_cost(r, seen.len(), t, list + list2, 2);
        }
    }

    if gold_vec.len() >= 2 {
        let anchor = gold_vec[0].clone();
        let rest: BTreeSet<String> = gold_vec[1..].iter().cloned().collect();
        for (name, t) in [("ubis anchor", ""), ("ubis anchor+text", text)] {
            let (r, n, tok, _, list) = run(store, &query(t, Some(anchor.clone())), &rest, args)?;
            aggs.entry(name).or_default().add_cost(r, n, tok, list, 1);
            if args.verbose {
                eprintln!("[{name}] {:.2} {:>3} | {} | anchor {}", r, n, subject, anchor);
            }
        }
        // Baseline: open the anchor's whole file.
        let path = store.unit(&anchor)?.map(|u| u.path).unwrap_or_default();
        let covered = rest
            .iter()
            .filter(|g| store.unit(g).ok().flatten().is_some_and(|u| u.path == path))
            .count();
        let t = files.texts.get(&path).map(|s| tokens_of(s)).unwrap_or(0.0);
        aggs.entry("read-anchor-file")
            .or_default()
            .add(covered as f64 / rest.len() as f64, 1, t);
    }
    Ok(())
}

/// Run one query; returns (recall, number returned, tokens to read the spans).
fn run(
    store: &Store,
    q: &Query,
    gold: &BTreeSet<String>,
    args: &Args,
) -> Result<(f64, usize, f64, Vec<String>, f64)> {
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
    let ids: Vec<String> = resp.hits.iter().map(|h| h.id.clone()).collect();
    let (r, t) = score(store, &ids, gold)?;
    Ok((r, ids.len(), t, ids, tokens_of(&ubis_core::render::hits(&resp.hits))))
}

/// Recall of `gold` by returned units (a hit covers its descendants) and the
/// tokens needed to read every returned span.
fn score(store: &Store, hits: &[String], gold: &BTreeSet<String>) -> Result<(f64, f64)> {
    let hit_ids: BTreeSet<&str> = hits.iter().map(String::as_str).collect();
    let mut found = 0;
    for g in gold {
        let covered = hit_ids.contains(g.as_str())
            || store.ancestors(g)?.iter().any(|a| hit_ids.contains(a.id.as_str()));
        if covered {
            found += 1;
        }
    }
    let mut tokens = 0.0;
    for h in hits {
        for u in store.subtree(h)? {
            if u.is_leaf {
                tokens += tokens_of(&u.text);
            }
        }
    }
    Ok((found as f64 / gold.len() as f64, tokens))
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

/// Term counts per file, computed once for the grep baseline.
fn term_counts(texts: &HashMap<String, String>) -> HashMap<String, HashMap<String, usize>> {
    texts
        .iter()
        .map(|(path, text)| {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for t in tokenize(text) {
                *counts.entry(t).or_default() += 1;
            }
            (path.clone(), counts)
        })
        .collect()
}

/// Baseline: rank files by distinct query terms present (ties: total
/// occurrences, then path), read the top `files` files whole.
fn grep_read(
    subject: &str,
    n_files: usize,
    gold: &BTreeSet<String>,
    files: &Files,
    store: &Store,
) -> Result<(f64, f64)> {
    let mut terms = tokenize(subject);
    terms.sort();
    terms.dedup();
    let mut ranked: Vec<(usize, usize, &String)> = files
        .terms
        .iter()
        .map(|(path, counts)| {
            let distinct = terms.iter().filter(|t| counts.contains_key(t.as_str())).count();
            let total: usize = terms.iter().map(|t| counts.get(t.as_str()).copied().unwrap_or(0)).sum();
            (distinct, total, path)
        })
        .filter(|(d, _, _)| *d > 0)
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    let chosen: BTreeSet<&str> = ranked.iter().take(n_files).map(|r| r.2.as_str()).collect();
    let mut found = 0;
    for g in gold {
        if store.unit(g)?.is_some_and(|u| chosen.contains(u.path.as_str())) {
            found += 1;
        }
    }
    let tokens: f64 = chosen.iter().map(|p| tokens_of(&files.texts[*p])).sum();
    Ok((found as f64 / gold.len() as f64, tokens))
}
