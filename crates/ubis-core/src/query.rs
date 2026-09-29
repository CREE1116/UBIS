//! Query cascade.
//!
//! ```text
//! Planner ─► Stage A: operators generate candidates (union)
//!        ─► Stage B: s(j) = Σ_f w_f · φ_f(j),  φ_f = raw_f / max raw_f
//!        ─► Stage C: collapse overlaps, lift crowded siblings, adaptive K
//! ```
//!
//! Every operator is deterministic. New signals (term expansion, spectral
//! relation scores, co-change) plug in as further [`Operator`]s and enter the
//! default plan only after the evaluation harness shows a gain.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use serde::Serialize;

use crate::cochange::CoChange;
use crate::model::*;
use crate::store::{Store, UnitRow};
use crate::fields;
use crate::tokenize::tokenize;

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    /// Unit the agent is currently looking at, if any.
    pub anchor: Option<String>,
    /// Restrict results to paths with this prefix.
    pub scope: Option<String>,
    pub k_max: usize,
    /// Lower bound of the adaptive cut. `None` uses [`K_MIN`]; `Some(k_max)`
    /// disables the adaptive cut.
    pub k_min: Option<usize>,
}

impl Query {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            anchor: None,
            scope: None,
            k_max: 10,
            k_min: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Via {
    pub op: &'static str,
    pub contribution: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: UnitId,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: UnitKind,
    pub label: String,
    pub signature: String,
    pub score: f64,
    pub via: Vec<Via>,
    /// Number of child hits folded into this one by sibling lifting.
    pub lifted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub plan: Vec<(&'static str, f64)>,
    pub candidates: usize,
    /// Source units: what to read or change.
    pub hits: Vec<Hit>,
    /// Test units, listed apart so they do not crowd out the source list.
    pub tests: Vec<Hit>,
}

/// Resolved query context shared by operators.
pub struct Context {
    /// The anchor and all of its descendants.
    pub anchor_set: BTreeSet<UnitId>,
    pub anchor: Option<UnitRow>,
}

/// Named parts of a score, e.g. `[("code", 3.1), ("name", 1.2)]`.
pub type Parts = Vec<(&'static str, f64)>;
/// A candidate with its score and the parts it is made of.
pub type Scored = (UnitId, f64, Parts);

pub trait Operator {
    fn name(&self) -> &'static str;
    /// Candidate units with a raw score (higher is better).
    fn generate(&self, store: &Store, q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>>;
    /// Scores that are already bits of evidence: added as they are when the
    /// operator is alone, instead of being scaled to its maximum.
    fn raw(&self) -> bool {
        false
    }
    /// Candidates with the named parts of their score (shown as `via`).
    fn generate_parts(&self, store: &Store, q: &Query, ctx: &Context) -> Result<Vec<Scored>> {
        let name = self.name();
        Ok(self.generate(store, q, ctx)?.into_iter().map(|(id, s)| (id, s, vec![(name, s)])).collect())
    }
}

// ------------------------------------------------------------------ operators

/// Bits of evidence from the query text, summed over the fields of
/// [`crate::fields`] (DPH per field; independent evidence adds). The `tests`
/// field scores files; its bits go to every candidate unit of the file, and
/// the best files also contribute their leaves.
pub struct Text {
    pub limit: usize,
}

/// Unit fields scored per leaf, in reporting order.
const UNIT_FIELDS: [&str; 4] = ["code", fields::NAME, fields::PATH, "history"];
/// Files whose leaves enter as candidates on `tests` bits alone.
const TEST_FILES: usize = 5;

impl Text {
    /// DPH bits of one field for the weighted query: doc → bits.
    fn field_bits(store: &Store, field: &str, terms: &[(String, f64)]) -> Result<BTreeMap<String, (f64, String)>> {
        let (n, total) = store.field_totals(field)?;
        let mut out: BTreeMap<String, (f64, String)> = BTreeMap::new();
        if n <= 0.0 || total <= 0.0 {
            return Ok(out);
        }
        let avg = total / n;
        for (term, qw) in terms {
            let postings: Vec<(String, f64, f64, String)> = match field {
                "code" => store.postings(term)?,
                "history" => store.hist_postings(term)?,
                _ => store.field_postings(field, term)?,
            };
            let cf: f64 = postings.iter().map(|p| p.1).sum();
            for (doc, tf, len, path) in postings {
                let w = qw * fields::dph(tf, len, avg, n, cf);
                let e = out.entry(doc).or_insert((0.0, path));
                e.0 += w;
            }
        }
        Ok(out)
    }

    /// Per unit, the bits of each field (`(field, bits)`, nonzero only).
    pub fn parts(&self, store: &Store, q: &Query) -> Result<Vec<Scored>> {
        let terms = weighted_terms(&q.text);
        let in_scope = |p: &str| q.scope.as_ref().is_none_or(|s| p.starts_with(s.as_str()));
        let mut units: BTreeMap<UnitId, (String, Parts)> = BTreeMap::new();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        for field in UNIT_FIELDS {
            for (id, (bits, path)) in Self::field_bits(store, field, &terms)? {
                if in_scope(&path) {
                    units.entry(id).or_insert_with(|| (path, Vec::new())).1.push((field, bits));
                }
            }
        }
        let files = Self::field_bits(store, fields::TESTS, &terms)?;
        let mut best: Vec<(&String, f64)> =
            files.iter().filter(|(p, (b, _))| *b > 0.0 && in_scope(p)).map(|(p, (b, _))| (p, *b)).collect();
        best.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (path, _) in best.into_iter().take(TEST_FILES) {
            for u in store.units_in_file(path)? {
                if u.is_leaf {
                    units.entry(u.id).or_insert_with(|| (path.clone(), Vec::new()));
                }
            }
        }
        let mut out: Vec<Scored> = units
            .into_iter()
            .map(|(id, (path, mut parts))| {
                if let Some((b, _)) = files.get(&path) {
                    if *b != 0.0 {
                        parts.push((fields::TESTS, *b));
                    }
                }
                let total = parts.iter().map(|p| p.1).sum();
                (id, total, parts)
            })
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(self.limit);
        Ok(out)
    }
}

/// Query terms weighted by how the asker used them: `1 + ln(qtf)` for words
/// repeated in the query, ×1.5 for words in the first line when there is
/// more (a task's title says what it is about; its body adds context and
/// noise). Measured on PR tasks (REPORT.md E11) and SWE-bench.
fn weighted_terms(text: &str) -> Vec<(String, f64)> {
    const TITLE_BOOST: f64 = 1.5;
    let mut qtf: BTreeMap<String, f64> = BTreeMap::new();
    for t in tokenize(text) {
        *qtf.entry(t).or_default() += 1.0;
    }
    let has_body = text.trim_end().contains('\n');
    let title: BTreeSet<String> = tokenize(text.lines().next().unwrap_or("")).into_iter().collect();
    qtf.into_iter()
        .map(|(t, c)| {
            let boost = if has_body && title.contains(&t) { TITLE_BOOST } else { 1.0 };
            let w = (1.0 + c.ln()) * boost;
            (t, w)
        })
        .collect()
}

impl Operator for Text {
    fn name(&self) -> &'static str {
        "text"
    }

    fn raw(&self) -> bool {
        true
    }

    fn generate(&self, store: &Store, q: &Query, _ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        Ok(self.parts(store, q)?.into_iter().map(|(id, s, _)| (id, s)).collect())
    }

    fn generate_parts(&self, store: &Store, q: &Query, _ctx: &Context) -> Result<Vec<Scored>> {
        self.parts(store, q)
    }
}

/// Units the anchor refers to (anchor → j), weighted by resolution mass.
pub struct RefsOut {
    pub limit: usize,
}

impl Operator for RefsOut {
    fn name(&self) -> &'static str {
        "refs_out"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for a in &ctx.anchor_set {
            for e in store.edges_from(a)? {
                if !ctx.anchor_set.contains(&e.dst) {
                    *scores.entry(e.dst).or_default() += e.weight;
                }
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Units that refer to the anchor (j → anchor).
pub struct RefsIn {
    pub limit: usize,
}

impl Operator for RefsIn {
    fn name(&self) -> &'static str {
        "refs_in"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for a in &ctx.anchor_set {
            for e in store.edges_to(a)? {
                if !ctx.anchor_set.contains(&e.src) {
                    *scores.entry(e.src).or_default() += e.weight;
                }
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Siblings of the anchor, closer in document order scores higher.
pub struct TreeNear {
    pub limit: usize,
}

impl Operator for TreeNear {
    fn name(&self) -> &'static str {
        "tree_near"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let Some(anchor) = &ctx.anchor else {
            return Ok(Vec::new());
        };
        let Some(parent) = &anchor.parent else {
            return Ok(Vec::new());
        };
        let mut scores = HashMap::new();
        for s in store.children(parent)? {
            if ctx.anchor_set.contains(&s.id) {
                continue;
            }
            let d = (s.ord - anchor.ord).unsigned_abs() as f64;
            scores.insert(s.id, 1.0 / (1.0 + d));
        }
        Ok(top(scores, self.limit))
    }
}

/// Other leaves in the anchor's file, closer in document order scores higher.
/// Much co-change is intra-file; this reaches it at unit granularity instead
/// of reading the whole file.
pub struct SameFile {
    pub limit: usize,
}

impl Operator for SameFile {
    fn name(&self) -> &'static str {
        "same_file"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let Some(anchor) = &ctx.anchor else {
            return Ok(Vec::new());
        };
        let mut scores = HashMap::new();
        for u in store.units_in_file(&anchor.path)? {
            if !u.is_leaf || ctx.anchor_set.contains(&u.id) {
                continue;
            }
            let d = (u.ord - anchor.ord).unsigned_abs() as f64;
            scores.insert(u.id, 1.0 / (1.0 + d / 4.0));
        }
        Ok(top(scores, self.limit))
    }
}

fn top(scores: HashMap<UnitId, f64>, limit: usize) -> Vec<(UnitId, f64)> {
    let mut v: Vec<_> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}

// -------------------------------------------------------------------- planner

pub struct Plan {
    pub ops: Vec<(Box<dyn Operator>, f64)>,
}

/// Deterministic routing: the text alone, or the anchor's neighbourhood
/// refined by the text.
pub fn plan(q: &Query) -> Plan {
    let mut ops: Vec<(Box<dyn Operator>, f64)> = Vec::new();
    if !q.text.trim().is_empty() {
        // With an anchor, the anchor is the stronger evidence; text refines.
        // (Measured with ubis-bench: text at full weight drowned anchor signals.)
        let w = if q.anchor.is_some() { ANCHOR_TEXT_WEIGHT } else { 1.0 };
        ops.push((Box::new(Text { limit: 400 }), w));
    }
    if q.anchor.is_some() {
        ops.push((Box::new(RefsIn { limit: 50 }), 0.8));
        ops.push((Box::new(RefsOut { limit: 50 }), 0.8));
        ops.push((Box::new(TreeNear { limit: 30 }), 0.3));
        ops.push((Box::new(SameFile { limit: 30 }), 0.2));
        // Empty outside git repositories; measured in REPORT.md E1.
        ops.push((Box::new(CoChange { limit: 50 }), 0.5));
    }
    Plan { ops }
}

// ------------------------------------------------------------------- cascade

/// Weight of the (max-scaled) text evidence next to an anchor.
pub const ANCHOR_TEXT_WEIGHT: f64 = 0.25;
/// Test units are listed apart from the source list, at most this many.
/// Measured (SWE-bench): tests outranked the unit to change in 69% of tasks.
pub const TESTS_K: usize = 3;
/// Contributions below this are left out of `via` (they would print as 0.00).
pub const VIA_MIN: f64 = 0.005;
pub const LIFT_MIN_SIBLINGS: usize = 4;
pub const LIFT_MAX_LINES: usize = 300;
/// Floor of the adaptive cut. Measured with ubis-bench: 3 cut anchor queries
/// too early (fd anchor recall 0.322 → 0.351 at 5), and candidates are cheap.
pub const K_MIN: usize = 5;
pub const MASS_TAU: f64 = 0.5;

pub fn search(store: &Store, q: &Query) -> Result<Response> {
    search_with(store, q, plan(q))
}

pub fn search_with(store: &Store, q: &Query, plan: Plan) -> Result<Response> {
    let anchor = match &q.anchor {
        Some(a) => store.find_unit(a)?.into_iter().next(),
        None => None,
    };
    let anchor_set: BTreeSet<UnitId> = match &anchor {
        Some(a) => store.subtree(&a.id)?.into_iter().map(|u| u.id).collect(),
        None => BTreeSet::new(),
    };
    let ctx = Context { anchor_set, anchor };

    // Stage A + B. Bits add as they are when the text is the only evidence;
    // otherwise every operator is scaled to its maximum and weighted.
    let alone = plan.ops.len() == 1;
    let mut total: BTreeMap<UnitId, (f64, Vec<Via>)> = BTreeMap::new();
    let mut plan_desc = Vec::new();
    for (op, w) in &plan.ops {
        plan_desc.push((op.name(), *w));
        let cands = op.generate_parts(store, q, &ctx)?;
        let max = cands.iter().map(|c| c.1).fold(0.0, f64::max);
        let scale = if op.raw() && alone {
            *w
        } else if max > 0.0 {
            w / max
        } else {
            continue;
        };
        for (id, raw, parts) in cands {
            let e = total.entry(id).or_insert((0.0, Vec::new()));
            e.0 += raw * scale;
            for (name, p) in parts {
                let c = p * scale;
                // Negligible evidence still counts in the score but is not shown.
                if c >= VIA_MIN {
                    e.1.push(Via { op: name, contribution: c });
                }
            }
        }
    }
    let candidates = total.len();

    let mut ranked: Vec<(UnitRow, f64, Vec<Via>, usize)> = Vec::new();
    for (id, (score, via)) in total {
        if let Some(u) = store.unit(&id)? {
            if let Some(scope) = &q.scope {
                if !u.path.starts_with(scope.as_str()) {
                    continue;
                }
            }
            ranked.push((u, score, via, 0));
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
    ranked.truncate(200);

    // Stage C, for the source list and the test list apart.
    let (tests, sources): (Ranked, Ranked) =
        ranked.into_iter().partition(|r| fields::is_test(&r.0.id, &r.0.path));
    let sources = collapse_ancestors(store, sources)?;
    let sources = lift_siblings(store, sources, q.k_max)?;
    let scores: Vec<f64> = sources.iter().map(|r| r.1.max(0.0)).collect();
    let k = adaptive_k_range(&scores, q.k_min.unwrap_or(K_MIN), q.k_max);
    let tests = collapse_ancestors(store, tests)?;
    let n_tests = tests.iter().take(TESTS_K).filter(|r| r.1 > 0.0).count();

    Ok(Response {
        plan: plan_desc,
        candidates,
        hits: sources.into_iter().take(k).map(to_hit).collect(),
        tests: tests.into_iter().take(n_tests).map(to_hit).collect(),
    })
}

fn to_hit((u, score, via, lifted): (UnitRow, f64, Vec<Via>, usize)) -> Hit {
    Hit {
        signature: u.signature(),
        id: u.id,
        path: u.path,
        start_line: u.start_line,
        end_line: u.end_line,
        kind: u.kind,
        label: u.label,
        score,
        via,
        lifted,
    }
}

type Ranked = Vec<(UnitRow, f64, Vec<Via>, usize)>;

/// Drop a hit when one of its descendants is also a hit: the smaller span is
/// the more useful answer, and the parent is reachable by zooming out.
fn collapse_ancestors(store: &Store, ranked: Ranked) -> Result<Ranked> {
    let ids: BTreeSet<UnitId> = ranked.iter().map(|r| r.0.id.clone()).collect();
    let mut covered = BTreeSet::new();
    for r in &ranked {
        for a in store.ancestors(&r.0.id)? {
            if ids.contains(&a.id) {
                covered.insert(a.id);
            }
        }
    }
    Ok(ranked
        .into_iter()
        .filter(|r| !covered.contains(&r.0.id))
        .collect())
}

/// When many hits share one small parent, return the parent once instead.
fn lift_siblings(store: &Store, ranked: Ranked, k_max: usize) -> Result<Ranked> {
    let window = (k_max * 2).min(ranked.len());
    let mut groups: BTreeMap<UnitId, Vec<usize>> = BTreeMap::new();
    for (i, r) in ranked.iter().take(window).enumerate() {
        if let Some(p) = &r.0.parent {
            groups.entry(p.clone()).or_default().push(i);
        }
    }
    let mut replace: BTreeMap<usize, UnitRow> = BTreeMap::new();
    let mut drop: BTreeSet<usize> = BTreeSet::new();
    for (parent, members) in groups {
        if members.len() < LIFT_MIN_SIBLINGS {
            continue;
        }
        let Some(p) = store.unit(&parent)? else { continue };
        if p.end_line + 1 - p.start_line > LIFT_MAX_LINES {
            continue;
        }
        replace.insert(members[0], p);
        drop.extend(members[1..].iter().copied());
    }
    if replace.is_empty() {
        return Ok(ranked);
    }
    let mut members_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (first, p) in &replace {
        let group: Vec<usize> = ranked
            .iter()
            .take(window)
            .enumerate()
            .filter(|(_, r)| r.0.parent.as_deref() == Some(p.id.as_str()))
            .map(|(i, _)| i)
            .collect();
        members_of.insert(*first, group);
    }
    let mut out = Vec::with_capacity(ranked.len());
    let snapshot: Vec<(f64, Vec<Via>)> = ranked.iter().map(|r| (r.1, r.2.clone())).collect();
    for (i, r) in ranked.into_iter().enumerate() {
        if drop.contains(&i) {
            continue;
        }
        if let Some(p) = replace.remove(&i) {
            let group = &members_of[&i];
            let mut via: BTreeMap<&'static str, f64> = BTreeMap::new();
            for &g in group {
                for v in &snapshot[g].1 {
                    let e = via.entry(v.op).or_default();
                    *e = e.max(v.contribution);
                }
            }
            let via = via
                .into_iter()
                .map(|(op, contribution)| Via { op, contribution })
                .collect();
            out.push((p, snapshot[i].0, via, group.len()));
        } else {
            out.push(r);
        }
    }
    Ok(out)
}

/// Cut at the largest score drop within `[K_MIN, k_max]`, subject to keeping
/// at least `MASS_TAU` of the score mass of the top `k_max`.
pub fn adaptive_k(scores: &[f64], k_max: usize) -> usize {
    adaptive_k_range(scores, K_MIN, k_max)
}

pub fn adaptive_k_range(scores: &[f64], k_min: usize, k_max: usize) -> usize {
    let hi = k_max.min(scores.len());
    if hi == 0 {
        return 0;
    }
    let lo = k_min.clamp(1, hi);
    let mass: f64 = scores[..hi].iter().sum();
    let mut best = hi;
    let mut best_gap = f64::NEG_INFINITY;
    let mut cum: f64 = scores[..lo - 1].iter().sum();
    for k in lo..=hi {
        cum += scores[k - 1];
        if mass > 0.0 && cum / mass < MASS_TAU {
            continue;
        }
        let next = scores.get(k).copied().unwrap_or(0.0);
        let gap = scores[k - 1] - next;
        // Ties go to the larger K: a flat score profile means the ranking is
        // unsure, so showing more candidates is the cheaper mistake.
        if gap >= best_gap - 1e-12 {
            best_gap = gap;
            best = k;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_k_cuts_at_largest_gap() {
        let s = [1.0, 0.95, 0.9, 0.2, 0.1, 0.05];
        assert_eq!(adaptive_k_range(&s, 3, 10), 3);
        assert_eq!(adaptive_k_range(&[1.0, 0.1, 0.09], 3, 10), 3); // floor
        assert_eq!(adaptive_k(&[1.0; 20], 10), 10); // flat: take everything allowed
        assert_eq!(adaptive_k(&[], 10), 0);
        assert_eq!(adaptive_k_range(&s, 10, 10), 6); // floor = max disables the cut
    }

    #[test]
    fn adaptive_k_respects_mass() {
        // Largest gap after the 3rd, but the first 3 hold < 50% of the mass.
        let s: [f64; 12] = [0.3, 0.3, 0.3, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05];
        let mut long = s.to_vec();
        long.extend([0.29; 4]);
        long.sort_by(|a, b| b.total_cmp(a));
        let k = adaptive_k(&long, 10);
        let mass: f64 = long[..10].iter().sum();
        assert!(long[..k].iter().sum::<f64>() / mass >= MASS_TAU);
    }
}
