//! Destructive-by-index confirmation prompt.
//!
//! When a destructive lifecycle command (rm, kill) resolved any of its
//! targets through an index selector, print the resolved rows and ask
//! the operator to confirm before proceeding. `-f/--force` skips the
//! prompt. Pure-literal targets (everyone an ID/name) skip the prompt
//! too: the operator already typed the literal, no surprise possible.
//!
//! Default-and-document: this module is consumed by `lifecycle_cmd`'s
//! rm/kill handlers in Task 7 of the Phase 0.22 plan; until those land
//! in the same PR, `dead_code` is allowed module-wide so clippy stays
//! green between commits.

#![allow(dead_code)]

use std::io::{BufRead, Write};

use anyhow::Result;

use crate::index_resolve::ResolvedTarget;

/// Decide whether the operator should be prompted, then prompt.
/// Returns `Ok(true)` to proceed, `Ok(false)` to abort (operator said
/// no or stdin closed before a `y`).
pub fn confirm_destructive(verb: &str, targets: &[ResolvedTarget], force: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    if !targets.iter().any(|t| t.via_index) {
        return Ok(true);
    }
    print_summary(verb, targets, &mut std::io::stderr())?;
    read_yes_no(&mut std::io::stdin().lock())
}

fn print_summary(verb: &str, targets: &[ResolvedTarget], w: &mut impl Write) -> Result<()> {
    writeln!(w, "isd: about to {verb} {} container(s):", targets.len())?;
    for t in targets {
        let short_id: String = t.container_id.chars().take(12).collect();
        if t.context.is_empty() {
            writeln!(w, "  {short_id}  {}", t.name)?;
        } else {
            writeln!(w, "  {short_id}  {} ({})", t.name, t.context)?;
        }
    }
    write!(w, "proceed? [y/N] ")?;
    w.flush()?;
    Ok(())
}

fn read_yes_no(r: &mut impl BufRead) -> Result<bool> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Ok(false);
    }
    let trimmed = line.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn target(via_index: bool) -> ResolvedTarget {
        ResolvedTarget {
            container_id: "a1b2c3d4e5f6".into(),
            name: "web-proxy".into(),
            context: "lausanne".into(),
            via_index,
        }
    }

    #[test]
    fn force_bypasses_prompt() {
        let yes = confirm_destructive("rm", &[target(true)], true).unwrap();
        assert!(yes);
    }

    #[test]
    fn literal_only_skips_prompt() {
        // No via_index = no surprise = no prompt.
        let yes = confirm_destructive("rm", &[target(false)], false).unwrap();
        assert!(yes);
    }

    #[test]
    fn print_summary_includes_id_name_and_context() {
        let mut buf = Vec::new();
        print_summary("rm", &[target(true)], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("rm"), "verb present");
        assert!(out.contains("a1b2c3d4e5f6"), "short id present");
        assert!(out.contains("web-proxy"), "name present");
        assert!(out.contains("lausanne"), "context present");
        assert!(out.contains("[y/N]"), "prompt present");
    }

    #[test]
    fn read_yes_no_accepts_y_yes_case_insensitive() {
        assert!(read_yes_no(&mut Cursor::new(b"y\n")).unwrap());
        assert!(read_yes_no(&mut Cursor::new(b"Y\n")).unwrap());
        assert!(read_yes_no(&mut Cursor::new(b"yes\n")).unwrap());
        assert!(read_yes_no(&mut Cursor::new(b"YES\n")).unwrap());
    }

    #[test]
    fn read_yes_no_rejects_n_anything_empty() {
        assert!(!read_yes_no(&mut Cursor::new(b"n\n")).unwrap());
        assert!(!read_yes_no(&mut Cursor::new(b"\n")).unwrap());
        assert!(!read_yes_no(&mut Cursor::new(b"")).unwrap());
        assert!(!read_yes_no(&mut Cursor::new(b"maybe\n")).unwrap());
    }
}
