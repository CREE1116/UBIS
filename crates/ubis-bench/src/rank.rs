//! `ubis-rank <db> [k]`: read JSON lines `{"id": .., "text": ..}` on stdin,
//! write `{"id": .., "hits": [unit ids], "tests": [unit ids]}` per line: the
//! top `k` source units (default 20) without the adaptive cut. For
//! evaluating the ranking outside this crate (the UBIS-V2 SWE-bench lab).

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use ubis_core::query::{search, Query};
use ubis_core::Store;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db = args.get(1).context("usage: ubis-rank <db> [k]")?;
    let k: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(20);
    let store = Store::open(std::path::Path::new(db))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        let q = Query {
            text: v["text"].as_str().unwrap_or("").to_string(),
            anchor: None,
            scope: None,
            k_max: k,
            k_min: Some(k),
        };
        let r = search(&store, &q)?;
        let ids = |h: &[ubis_core::Hit]| h.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        let o = serde_json::json!({"id": v["id"], "hits": ids(&r.hits), "tests": ids(&r.tests)});
        writeln!(out, "{o}")?;
    }
    Ok(())
}
