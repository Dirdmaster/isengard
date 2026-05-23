//! Short rendering of a host ULID for human-readable tables.
//!
//! ULIDs are 26 chars in Crockford base32. Chars 1-10 encode the
//! enroll timestamp (millisecond resolution); chars 11-26 are random
//! (80 bits of entropy). Rendering the full string in every table
//! buries the operator in noise that's only useful when scripting.
//!
//! The display convention here is the random suffix (last 8 chars):
//!
//!   `01KS1BN0FMTPTABT3BTYW0B8M8` -> `TYW0B8M8`
//!
//! Same mental model as `docker ps`'s short container hash. The 40
//! bits sampled are uniform random so collisions need ~1 million hosts
//! before the birthday bound kicks in (more than a homelab will ever
//! see).
//!
//! Operators who need the full ULID get it via `--full-id` on the
//! relevant CLI or `--format json` (which always renders verbatim).

/// Length of the rendered short suffix. Eight chars matches the
/// docker-ps mental model and stays inside the boxed table chrome
/// even on narrow terminals.
pub const SHORT_HOST_ID_LEN: usize = 8;

/// Render a host ULID as its short suffix. Strings shorter than
/// [`SHORT_HOST_ID_LEN`] (truncated IDs, test fixtures) pass through
/// unchanged so callers don't have to special-case them.
pub fn short(id: &str) -> String {
    if id.len() <= SHORT_HOST_ID_LEN {
        return id.to_string();
    }
    id.chars()
        .skip(id.chars().count().saturating_sub(SHORT_HOST_ID_LEN))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_takes_last_eight_chars_of_a_ulid() {
        let id = "01KS1BN0FMTPTABT3BTYW0B8M8";
        assert_eq!(short(id), "TYW0B8M8");
        assert_eq!(short(id).len(), SHORT_HOST_ID_LEN);
    }

    #[test]
    fn short_passes_through_strings_shorter_than_the_window() {
        assert_eq!(short(""), "");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("12345678"), "12345678");
    }

    #[test]
    fn short_handles_strings_at_the_window_boundary() {
        // Exactly 8 chars: nothing to truncate.
        assert_eq!(short("12345678"), "12345678");
        // Nine chars: drop the leading one.
        assert_eq!(short("123456789"), "23456789");
    }
}
