//! Deterministic tokenizer shared by indexing and querying.
//!
//! * Latin/ASCII words: the lowercased word plus its `snake_case` and
//!   `camelCase` parts (`HnswIndex` → `hnswindex`, `hnsw`, `index`).
//!   Purely alphabetic terms are reduced to their English stem (Snowball),
//!   so `selection` meets `select` and `routing` meets `route`. Whole
//!   identifiers with `_` or digits are kept verbatim.
//! * Hangul and CJK runs: character bigrams, so that `검색은` matches `검색`
//!   without a morphological analyzer. A single-character run is kept whole.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Cjk,
    Other,
}

fn class(c: char) -> Class {
    if is_cjk(c) {
        Class::Cjk
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Other
    }
}

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0xAC00..=0xD7A3   // Hangul syllables
        | 0x1100..=0x11FF // Hangul jamo
        | 0x3130..=0x318F // Hangul compatibility jamo
        | 0x3040..=0x30FF // Hiragana, Katakana
        | 0x4E00..=0x9FFF // CJK unified ideographs
    )
}

/// Tokenize text into index terms. Order follows the text; duplicates are kept
/// so that term frequency can be counted by the caller.
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_with(text, true)
}

/// Surface forms only, no stems. For judging and baselines (gold mapping,
/// grep), which must not move when retrieval's tokenization changes.
pub fn tokenize_raw(text: &str) -> Vec<String> {
    tokenize_with(text, false)
}

fn tokenize_with(text: &str, stems: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_class = Class::Other;
    for c in text.chars() {
        let k = class(c);
        if k != run_class && !run.is_empty() {
            flush(&run, run_class, stems, &mut out);
            run.clear();
        }
        run_class = k;
        if k != Class::Other {
            run.push(c);
        }
    }
    if !run.is_empty() {
        flush(&run, run_class, stems, &mut out);
    }
    out
}

fn flush(run: &str, class: Class, stems: bool, out: &mut Vec<String>) {
    match class {
        Class::Word => word_terms(run, stems, out),
        Class::Cjk => {
            let chars: Vec<char> = run.chars().collect();
            if chars.len() == 1 {
                out.push(chars[0].to_string());
            } else {
                for w in chars.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        }
        Class::Other => {}
    }
}

fn word_terms(word: &str, stems: bool, out: &mut Vec<String>) {
    let trimmed = word.trim_matches('_');
    if trimmed.is_empty() {
        return;
    }
    let full = trimmed.to_lowercase();
    let parts = split_identifier(trimmed);
    if full.chars().count() >= 2 {
        push_with_stem(&full, stems, out);
    }
    if parts.len() > 1 {
        for p in parts {
            if p.chars().count() >= 2 && p != full {
                push_with_stem(&p, stems, out);
            }
        }
    }
}

/// The term itself, plus its stem when that differs: an exact form matches
/// twice (form and stem), an inflected form still meets through the stem.
fn push_with_stem(term: &str, stems: bool, out: &mut Vec<String>) {
    if stems {
        let s = stem(term);
        if s != term {
            out.push(s);
        }
    }
    out.push(term.to_string());
}

/// English stem of a lowercase, purely alphabetic ASCII term; anything else
/// (identifiers with `_` or digits, non-Latin) is returned unchanged.
pub fn stem(term: &str) -> String {
    use std::sync::OnceLock;
    static STEMMER: OnceLock<rust_stemmers::Stemmer> = OnceLock::new();
    if term.len() < 4 || !term.bytes().all(|b| b.is_ascii_lowercase()) {
        return term.to_string();
    }
    let s = STEMMER.get_or_init(|| rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English));
    s.stem(term).into_owned()
}

/// Split an identifier on `_` and case boundaries, lowercased.
/// `parseHTTPResponse2` → `parse`, `http`, `response2`.
pub fn split_identifier(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for piece in word.split('_').filter(|p| !p.is_empty()) {
        let chars: Vec<char> = piece.chars().collect();
        let mut cur = String::new();
        for i in 0..chars.len() {
            let c = chars[i];
            if i > 0 && c.is_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower)
                {
                    parts.push(std::mem::take(&mut cur).to_lowercase());
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            parts.push(cur.to_lowercase());
        }
    }
    parts
}

/// True when a token looks like a code identifier rather than an English word:
/// contains `_`, an inner capital, `::`, or a `.` between identifier parts.
pub fn is_identifier_shaped(token: &str) -> bool {
    let t = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if t.chars().count() < 3 {
        return false;
    }
    if t.contains("::") || t.contains('_') {
        return true;
    }
    let chars: Vec<char> = t.chars().collect();
    chars
        .windows(2)
        .any(|w| w[0].is_lowercase() && w[1].is_uppercase())
        || (chars[0].is_uppercase() && chars.iter().skip(1).any(|c| c.is_uppercase()) && chars.iter().any(|c| c.is_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_split() {
        assert_eq!(
            tokenize("HnswIndex::search_layer"),
            vec!["hnswindex", "hnsw", "index", "search_layer", "search", "layer"]
        );
        assert_eq!(split_identifier("parseHTTPResponse2"), vec!["parse", "http", "response2"]);
    }

    #[test]
    fn hangul_bigrams_match_inflections() {
        let doc = tokenize("검색은 빠르다");
        let q = tokenize("검색");
        assert!(q.iter().all(|t| doc.contains(t)));
        assert_eq!(tokenize("책"), vec!["책"]);
    }

    #[test]
    fn mixed_scripts() {
        assert_eq!(tokenize("BM25로 검색"), vec!["bm25", "로", "검색"]);
    }

    #[test]
    fn identifier_shape() {
        assert!(is_identifier_shaped("HnswIndex"));
        assert!(is_identifier_shaped("ef_search"));
        assert!(is_identifier_shaped("Store::open"));
        assert!(!is_identifier_shaped("Search"));
        assert!(!is_identifier_shaped("index"));
    }

    #[test]
    fn raw_has_no_stems() {
        assert!(tokenize("selection routing").contains(&"select".to_string()));
        assert_eq!(tokenize_raw("selection routing"), vec!["selection", "routing"]);
    }
}
