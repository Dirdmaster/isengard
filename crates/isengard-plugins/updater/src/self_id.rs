//! Detect this process's own container ID by reading /proc/self/cgroup.
//!
//! Returns `None` on non-Linux platforms (e.g. macOS dev) and on Linux hosts
//! where the agent runs outside any container (the cgroup file then contains
//! no docker/containerd-style paths).
//!
//! Used by the updater to refuse to recreate its own container — phase 3d
//! adds proper rename-first self-update; until then, "skip self" is the
//! safe default.

use std::fs;

/// Returns the long (64-hex) container ID this process is running inside,
/// or `None` if not detectable.
pub fn current_container_id() -> Option<String> {
    let bytes = fs::read("/proc/self/cgroup").ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    parse_cgroup(text)
}

fn parse_cgroup(text: &str) -> Option<String> {
    // Lines look like: "12:devices:/docker/<64-hex>" or
    //                  "0::/system.slice/docker-<64-hex>.scope"
    // We pull out the 64-hex segment. Ignore short IDs (12-hex truncations).
    for line in text.lines() {
        if let Some(id) = extract_64_hex(line) {
            return Some(id);
        }
    }
    None
}

fn extract_64_hex(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut start = 0;
    while start + 64 <= bytes.len() {
        let candidate = &bytes[start..start + 64];
        if candidate.iter().all(|b| b.is_ascii_hexdigit()) {
            // Boundary check: char before must be non-hex (or start of slice),
            // char after must be non-hex (or end).
            let before_ok = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
            let after_ok = start + 64 == bytes.len() || !bytes[start + 64].is_ascii_hexdigit();
            if before_ok && after_ok {
                return Some(std::str::from_utf8(candidate).unwrap().to_string());
            }
        }
        start += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_classic_docker_cgroup_line() {
        let line =
            "12:devices:/docker/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(
            extract_64_hex(line).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn extract_from_systemd_scope_line() {
        let line = "0::/system.slice/docker-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210.scope";
        assert_eq!(
            extract_64_hex(line).as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
        );
    }

    #[test]
    fn ignores_short_truncated_id() {
        let line = "12:devices:/docker/abcdef012345";
        assert!(extract_64_hex(line).is_none());
    }

    #[test]
    fn ignores_random_hex_runs_too_long() {
        // 65 hex chars in a row should NOT match (boundary-after must be non-hex)
        let line = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789a";
        assert!(extract_64_hex(line).is_none());
    }

    #[test]
    fn parse_cgroup_picks_first_match() {
        let text = "10:cpu:/\n12:devices:/docker/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n";
        assert!(parse_cgroup(text).is_some());
    }

    #[test]
    fn parse_cgroup_returns_none_for_no_container() {
        let text = "10:cpu:/\n11:memory:/user.slice/user-1000.slice\n";
        assert!(parse_cgroup(text).is_none());
    }
}
