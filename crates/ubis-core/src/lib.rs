//! UBIS core — *Don't walk the graph. Project it.*
//!
//! * [`model`]: units, definitions, mentions, edges
//! * [`store`]: SQLite evidence store (the only source of truth)
//! * [`resolve`]: mentions ⋈ definitions → weighted edges
//! * [`query`]: planner and three-stage retrieval cascade
//! * [`tokenize`]: deterministic tokenizer for code, prose, and Hangul

pub mod model;
pub mod query;
pub mod resolve;
pub mod store;
pub mod tokenize;

pub use model::*;
pub use query::{search, Hit, Query, Response};
pub use store::{Stats, Store, UnitRow};
