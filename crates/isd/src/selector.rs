//! Parse a range/list selector for index-based row selection.
//!
//! Grammar (per `2026-05-15-isd-table-renderer-design.md`):
//!
//! ```text
//! selector := item (',' item)*
//! item     := N | N '-' M
//! ```
//!
//! Selectors are space-separated tokens on the command line. The parser
//! accepts one token at a time and is tolerant of whitespace inside an
//! item (`1 - 3` parses the same as `1-3`).
//!
//! Default-and-document: this module is consumed by `index_resolve.rs`
//! (Task 3 of the plan); until that lands in the same PR,
//! `dead_code` is allowed module-wide so clippy -D warnings stays green
//! between commits. Mirrors the pattern in `index_cache.rs`.

#![allow(dead_code)]

use anyhow::{Result, anyhow};

/// Parse one selector token into a sorted, deduped list of indices.
///
/// Accepts a single token (no spaces between top-level items): comma-
/// joined items, where each item is either a decimal `N` or a range
/// `lo-hi` (whitespace inside an item is tolerated). Output is sorted
/// ascending with duplicates removed so `3,1,2,2-3` collapses to
/// `[1, 2, 3]`.
///
/// # Errors
///
/// Returns `Err` on empty items, non-decimal characters, or inverted
/// ranges (`hi < lo`).
pub fn parse_token(s: &str) -> Result<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    for item in s.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(anyhow!("empty selector item in {s:?}"));
        }
        if let Some((lo, hi)) = item.split_once('-') {
            let lo: usize = lo
                .trim()
                .parse()
                .map_err(|e| anyhow!("invalid range lower bound in {item:?}: {e}"))?;
            let hi: usize = hi
                .trim()
                .parse()
                .map_err(|e| anyhow!("invalid range upper bound in {item:?}: {e}"))?;
            if hi < lo {
                return Err(anyhow!(
                    "range upper bound {hi} is less than lower bound {lo} in {item:?}"
                ));
            }
            for n in lo..=hi {
                out.push(n);
            }
        } else {
            let n: usize = item
                .parse()
                .map_err(|e| anyhow!("invalid index {item:?}: {e}"))?;
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Classify whether `s` looks like a selector token.
///
/// Returns true when `s` is non-empty and every char is a decimal
/// digit, `-`, `,`, or ASCII whitespace. The lifecycle commands use
/// this to decide between "resolve via the index cache" and "pass
/// through as a docker ID or name." A bare integer like `2` matches;
/// a name like `web-proxy` doesn't (hyphen alone is fine, but the
/// alpha chars disqualify it).
pub fn looks_like_selector(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ',' || c.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_index() {
        assert_eq!(parse_token("2").unwrap(), vec![2]);
    }

    #[test]
    fn comma_list() {
        assert_eq!(parse_token("1,3,5").unwrap(), vec![1, 3, 5]);
    }

    #[test]
    fn range() {
        assert_eq!(parse_token("1-3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn mixed_range_and_list() {
        assert_eq!(parse_token("1-3,5").unwrap(), vec![1, 2, 3, 5]);
    }

    #[test]
    fn deduped_and_sorted() {
        assert_eq!(parse_token("3,1,2,2-3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn rejects_inverted_range() {
        let err = parse_token("5-3").unwrap_err().to_string();
        assert!(err.contains("less than"), "got: {err}");
    }

    #[test]
    fn rejects_empty_item() {
        assert!(parse_token("1,,3").is_err());
        assert!(parse_token("").is_err());
    }

    #[test]
    fn rejects_non_decimal() {
        assert!(parse_token("a").is_err());
        assert!(parse_token("1-a").is_err());
    }

    #[test]
    fn looks_like_selector_matches_only_decimal_and_punctuation() {
        assert!(looks_like_selector("2"));
        assert!(looks_like_selector("1-3,5"));
        assert!(looks_like_selector(" 1 - 3 "));
        assert!(!looks_like_selector("a1b2c3"));
        assert!(!looks_like_selector("web-proxy"));
        assert!(!looks_like_selector(""));
    }
}
