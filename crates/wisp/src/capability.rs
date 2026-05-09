//! Capability set application for wisp containers.
//!
//! Per spec section "Capabilities", the OCI runtime spec exposes five
//! capability sets per process: `bounding`, `permitted`, `effective`,
//! `inheritable`, and `ambient`. Wisp applies them in this order during
//! container start (post `clone3`, around `pivot_root`):
//!
//! 1. drop bounding to spec.bounding
//! 2. set permitted = spec.permitted
//! 3. set effective = spec.effective
//! 4. set inheritable = spec.inheritable (required for ambient)
//! 5. raise ambient = spec.ambient (post pivot_root)
//!
//! The real syscalls live behind `#[cfg(target_os = "linux")]`. On
//! macOS the same public functions exist but are no-ops so the dev
//! loop compiles; the [`Capability`] type also has a Mac-only stub
//! enum with the same OCI string names so [`from_oci`] is exercised
//! identically by unit tests on both targets.

use crate::error::{Result, WispError};

/// Linux build re-exports the upstream [`caps::Capability`] verbatim.
/// On Mac (where the `caps` crate fails to compile because of
/// `prctl`/`capget` references) we provide an equivalent enum with
/// the same variant names and same `Display` / `FromStr` strings, so
/// the OCI-name parsing path is portable for unit tests.
#[cfg(target_os = "linux")]
pub use caps::Capability;

#[cfg(not(target_os = "linux"))]
pub use stub::Capability;

#[cfg(not(target_os = "linux"))]
mod stub {
    //! Mac-only `Capability` mirror. Variant set matches caps 0.5.6
    //! exactly so `from_oci` tests run cross-platform. Phase 0.1 only
    //! exercises the OCI string mapping on Mac; the syscall-modifying
    //! functions are `Ok(())` no-ops here.

    use std::fmt;
    use std::str::FromStr;

    #[allow(non_camel_case_types)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Capability {
        CAP_CHOWN,
        CAP_DAC_OVERRIDE,
        CAP_DAC_READ_SEARCH,
        CAP_FOWNER,
        CAP_FSETID,
        CAP_KILL,
        CAP_SETGID,
        CAP_SETUID,
        CAP_SETPCAP,
        CAP_LINUX_IMMUTABLE,
        CAP_NET_BIND_SERVICE,
        CAP_NET_BROADCAST,
        CAP_NET_ADMIN,
        CAP_NET_RAW,
        CAP_IPC_LOCK,
        CAP_IPC_OWNER,
        CAP_SYS_MODULE,
        CAP_SYS_RAWIO,
        CAP_SYS_CHROOT,
        CAP_SYS_PTRACE,
        CAP_SYS_PACCT,
        CAP_SYS_ADMIN,
        CAP_SYS_BOOT,
        CAP_SYS_NICE,
        CAP_SYS_RESOURCE,
        CAP_SYS_TIME,
        CAP_SYS_TTY_CONFIG,
        CAP_MKNOD,
        CAP_LEASE,
        CAP_AUDIT_WRITE,
        CAP_AUDIT_CONTROL,
        CAP_SETFCAP,
        CAP_MAC_OVERRIDE,
        CAP_MAC_ADMIN,
        CAP_SYSLOG,
        CAP_WAKE_ALARM,
        CAP_BLOCK_SUSPEND,
        CAP_AUDIT_READ,
        CAP_PERFMON,
        CAP_BPF,
        CAP_CHECKPOINT_RESTORE,
    }

    impl fmt::Display for Capability {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let name = match self {
                Capability::CAP_CHOWN => "CAP_CHOWN",
                Capability::CAP_DAC_OVERRIDE => "CAP_DAC_OVERRIDE",
                Capability::CAP_DAC_READ_SEARCH => "CAP_DAC_READ_SEARCH",
                Capability::CAP_FOWNER => "CAP_FOWNER",
                Capability::CAP_FSETID => "CAP_FSETID",
                Capability::CAP_KILL => "CAP_KILL",
                Capability::CAP_SETGID => "CAP_SETGID",
                Capability::CAP_SETUID => "CAP_SETUID",
                Capability::CAP_SETPCAP => "CAP_SETPCAP",
                Capability::CAP_LINUX_IMMUTABLE => "CAP_LINUX_IMMUTABLE",
                Capability::CAP_NET_BIND_SERVICE => "CAP_NET_BIND_SERVICE",
                Capability::CAP_NET_BROADCAST => "CAP_NET_BROADCAST",
                Capability::CAP_NET_ADMIN => "CAP_NET_ADMIN",
                Capability::CAP_NET_RAW => "CAP_NET_RAW",
                Capability::CAP_IPC_LOCK => "CAP_IPC_LOCK",
                Capability::CAP_IPC_OWNER => "CAP_IPC_OWNER",
                Capability::CAP_SYS_MODULE => "CAP_SYS_MODULE",
                Capability::CAP_SYS_RAWIO => "CAP_SYS_RAWIO",
                Capability::CAP_SYS_CHROOT => "CAP_SYS_CHROOT",
                Capability::CAP_SYS_PTRACE => "CAP_SYS_PTRACE",
                Capability::CAP_SYS_PACCT => "CAP_SYS_PACCT",
                Capability::CAP_SYS_ADMIN => "CAP_SYS_ADMIN",
                Capability::CAP_SYS_BOOT => "CAP_SYS_BOOT",
                Capability::CAP_SYS_NICE => "CAP_SYS_NICE",
                Capability::CAP_SYS_RESOURCE => "CAP_SYS_RESOURCE",
                Capability::CAP_SYS_TIME => "CAP_SYS_TIME",
                Capability::CAP_SYS_TTY_CONFIG => "CAP_SYS_TTY_CONFIG",
                Capability::CAP_MKNOD => "CAP_MKNOD",
                Capability::CAP_LEASE => "CAP_LEASE",
                Capability::CAP_AUDIT_WRITE => "CAP_AUDIT_WRITE",
                Capability::CAP_AUDIT_CONTROL => "CAP_AUDIT_CONTROL",
                Capability::CAP_SETFCAP => "CAP_SETFCAP",
                Capability::CAP_MAC_OVERRIDE => "CAP_MAC_OVERRIDE",
                Capability::CAP_MAC_ADMIN => "CAP_MAC_ADMIN",
                Capability::CAP_SYSLOG => "CAP_SYSLOG",
                Capability::CAP_WAKE_ALARM => "CAP_WAKE_ALARM",
                Capability::CAP_BLOCK_SUSPEND => "CAP_BLOCK_SUSPEND",
                Capability::CAP_AUDIT_READ => "CAP_AUDIT_READ",
                Capability::CAP_PERFMON => "CAP_PERFMON",
                Capability::CAP_BPF => "CAP_BPF",
                Capability::CAP_CHECKPOINT_RESTORE => "CAP_CHECKPOINT_RESTORE",
            };
            f.write_str(name)
        }
    }

    impl FromStr for Capability {
        type Err = String;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            match s {
                "CAP_CHOWN" => Ok(Capability::CAP_CHOWN),
                "CAP_DAC_OVERRIDE" => Ok(Capability::CAP_DAC_OVERRIDE),
                "CAP_DAC_READ_SEARCH" => Ok(Capability::CAP_DAC_READ_SEARCH),
                "CAP_FOWNER" => Ok(Capability::CAP_FOWNER),
                "CAP_FSETID" => Ok(Capability::CAP_FSETID),
                "CAP_KILL" => Ok(Capability::CAP_KILL),
                "CAP_SETGID" => Ok(Capability::CAP_SETGID),
                "CAP_SETUID" => Ok(Capability::CAP_SETUID),
                "CAP_SETPCAP" => Ok(Capability::CAP_SETPCAP),
                "CAP_LINUX_IMMUTABLE" => Ok(Capability::CAP_LINUX_IMMUTABLE),
                "CAP_NET_BIND_SERVICE" => Ok(Capability::CAP_NET_BIND_SERVICE),
                "CAP_NET_BROADCAST" => Ok(Capability::CAP_NET_BROADCAST),
                "CAP_NET_ADMIN" => Ok(Capability::CAP_NET_ADMIN),
                "CAP_NET_RAW" => Ok(Capability::CAP_NET_RAW),
                "CAP_IPC_LOCK" => Ok(Capability::CAP_IPC_LOCK),
                "CAP_IPC_OWNER" => Ok(Capability::CAP_IPC_OWNER),
                "CAP_SYS_MODULE" => Ok(Capability::CAP_SYS_MODULE),
                "CAP_SYS_RAWIO" => Ok(Capability::CAP_SYS_RAWIO),
                "CAP_SYS_CHROOT" => Ok(Capability::CAP_SYS_CHROOT),
                "CAP_SYS_PTRACE" => Ok(Capability::CAP_SYS_PTRACE),
                "CAP_SYS_PACCT" => Ok(Capability::CAP_SYS_PACCT),
                "CAP_SYS_ADMIN" => Ok(Capability::CAP_SYS_ADMIN),
                "CAP_SYS_BOOT" => Ok(Capability::CAP_SYS_BOOT),
                "CAP_SYS_NICE" => Ok(Capability::CAP_SYS_NICE),
                "CAP_SYS_RESOURCE" => Ok(Capability::CAP_SYS_RESOURCE),
                "CAP_SYS_TIME" => Ok(Capability::CAP_SYS_TIME),
                "CAP_SYS_TTY_CONFIG" => Ok(Capability::CAP_SYS_TTY_CONFIG),
                "CAP_MKNOD" => Ok(Capability::CAP_MKNOD),
                "CAP_LEASE" => Ok(Capability::CAP_LEASE),
                "CAP_AUDIT_WRITE" => Ok(Capability::CAP_AUDIT_WRITE),
                "CAP_AUDIT_CONTROL" => Ok(Capability::CAP_AUDIT_CONTROL),
                "CAP_SETFCAP" => Ok(Capability::CAP_SETFCAP),
                "CAP_MAC_OVERRIDE" => Ok(Capability::CAP_MAC_OVERRIDE),
                "CAP_MAC_ADMIN" => Ok(Capability::CAP_MAC_ADMIN),
                "CAP_SYSLOG" => Ok(Capability::CAP_SYSLOG),
                "CAP_WAKE_ALARM" => Ok(Capability::CAP_WAKE_ALARM),
                "CAP_BLOCK_SUSPEND" => Ok(Capability::CAP_BLOCK_SUSPEND),
                "CAP_AUDIT_READ" => Ok(Capability::CAP_AUDIT_READ),
                "CAP_PERFMON" => Ok(Capability::CAP_PERFMON),
                "CAP_BPF" => Ok(Capability::CAP_BPF),
                "CAP_CHECKPOINT_RESTORE" => Ok(Capability::CAP_CHECKPOINT_RESTORE),
                _ => Err(format!("invalid capability: {s}")),
            }
        }
    }
}

/// Every capability variant wisp recognises. Keeping our own list
/// (rather than relying on a `caps::iter_variants()` that doesn't
/// exist) lets the round-trip test catch upstream additions, and it
/// works identically on Mac via the stub enum. Test-only: the
/// production code parses by name on demand and never iterates the
/// full set.
#[cfg(test)]
const ALL_VARIANTS: &[Capability] = &[
    Capability::CAP_CHOWN,
    Capability::CAP_DAC_OVERRIDE,
    Capability::CAP_DAC_READ_SEARCH,
    Capability::CAP_FOWNER,
    Capability::CAP_FSETID,
    Capability::CAP_KILL,
    Capability::CAP_SETGID,
    Capability::CAP_SETUID,
    Capability::CAP_SETPCAP,
    Capability::CAP_LINUX_IMMUTABLE,
    Capability::CAP_NET_BIND_SERVICE,
    Capability::CAP_NET_BROADCAST,
    Capability::CAP_NET_ADMIN,
    Capability::CAP_NET_RAW,
    Capability::CAP_IPC_LOCK,
    Capability::CAP_IPC_OWNER,
    Capability::CAP_SYS_MODULE,
    Capability::CAP_SYS_RAWIO,
    Capability::CAP_SYS_CHROOT,
    Capability::CAP_SYS_PTRACE,
    Capability::CAP_SYS_PACCT,
    Capability::CAP_SYS_ADMIN,
    Capability::CAP_SYS_BOOT,
    Capability::CAP_SYS_NICE,
    Capability::CAP_SYS_RESOURCE,
    Capability::CAP_SYS_TIME,
    Capability::CAP_SYS_TTY_CONFIG,
    Capability::CAP_MKNOD,
    Capability::CAP_LEASE,
    Capability::CAP_AUDIT_WRITE,
    Capability::CAP_AUDIT_CONTROL,
    Capability::CAP_SETFCAP,
    Capability::CAP_MAC_OVERRIDE,
    Capability::CAP_MAC_ADMIN,
    Capability::CAP_SYSLOG,
    Capability::CAP_WAKE_ALARM,
    Capability::CAP_BLOCK_SUSPEND,
    Capability::CAP_AUDIT_READ,
    Capability::CAP_PERFMON,
    Capability::CAP_BPF,
    Capability::CAP_CHECKPOINT_RESTORE,
];

/// Translate OCI capability strings (e.g. "CAP_NET_BIND_SERVICE") into
/// the matching [`Capability`] values.
///
/// Garbage names are rejected with a [`WispError::Capability`] that
/// names the offending entry. Empty input produces an empty `Vec`.
/// Portable: this is the only function in the module exercised on
/// macOS unit tests.
pub fn from_oci(set: &[String]) -> Result<Vec<Capability>> {
    let mut out = Vec::with_capacity(set.len());
    for raw in set {
        let parsed = raw.parse::<Capability>().map_err(|_| {
            WispError::Capability(format!(
                "unknown OCI capability name: {raw:?}; expected something like CAP_NET_BIND_SERVICE"
            ))
        })?;
        out.push(parsed);
    }
    Ok(out)
}

/// Drop every capability not in `allowed` from the bounding set.
///
/// Linux: walks the current process's bounding set and drops each cap
/// that isn't in `allowed`. Capabilities already absent from bounding
/// are tolerated. Errors map to [`WispError::Capability`].
///
/// Non-Linux: no-op `Ok(())`. Unit tests on Mac never call this; the
/// stub keeps lifecycle wiring portable.
#[cfg(target_os = "linux")]
pub fn drop_bounding(allowed: &[Capability]) -> Result<()> {
    use std::collections::HashSet;

    let keep: HashSet<Capability> = allowed.iter().copied().collect();
    let current = caps::read(None, caps::CapSet::Bounding)
        .map_err(|err| WispError::Capability(format!("read bounding set: {err}")))?;
    for cap in current {
        if !keep.contains(&cap) {
            caps::drop(None, caps::CapSet::Bounding, cap)
                .map_err(|err| WispError::Capability(format!("drop {cap} from bounding: {err}")))?;
        }
    }
    Ok(())
}

/// Non-Linux stub: capability manipulation is a Linux-only concern,
/// the call returns `Ok(())` so callers can stay portable.
#[cfg(not(target_os = "linux"))]
pub fn drop_bounding(_allowed: &[Capability]) -> Result<()> {
    Ok(())
}

/// Set the thread's permitted set to exactly `perms`.
#[cfg(target_os = "linux")]
pub fn set_permitted(perms: &[Capability]) -> Result<()> {
    let value: caps::CapsHashSet = perms.iter().copied().collect();
    caps::set(None, caps::CapSet::Permitted, &value)
        .map_err(|err| WispError::Capability(format!("set permitted: {err}")))
}

#[cfg(not(target_os = "linux"))]
pub fn set_permitted(_perms: &[Capability]) -> Result<()> {
    Ok(())
}

/// Set the thread's effective set to exactly `eff`.
#[cfg(target_os = "linux")]
pub fn set_effective(eff: &[Capability]) -> Result<()> {
    let value: caps::CapsHashSet = eff.iter().copied().collect();
    caps::set(None, caps::CapSet::Effective, &value)
        .map_err(|err| WispError::Capability(format!("set effective: {err}")))
}

#[cfg(not(target_os = "linux"))]
pub fn set_effective(_eff: &[Capability]) -> Result<()> {
    Ok(())
}

/// Set the thread's inheritable set to exactly `inh`.
///
/// Inheritable must be set before ambient: a capability can only be
/// raised in ambient if it's already in inheritable AND permitted.
#[cfg(target_os = "linux")]
pub fn set_inheritable(inh: &[Capability]) -> Result<()> {
    let value: caps::CapsHashSet = inh.iter().copied().collect();
    caps::set(None, caps::CapSet::Inheritable, &value)
        .map_err(|err| WispError::Capability(format!("set inheritable: {err}")))
}

#[cfg(not(target_os = "linux"))]
pub fn set_inheritable(_inh: &[Capability]) -> Result<()> {
    Ok(())
}

/// Raise each cap in `amb` in the ambient set.
///
/// Internally `caps::raise` issues `prctl(PR_CAP_AMBIENT,
/// PR_CAP_AMBIENT_RAISE, cap, 0, 0)`. Each cap must already be in
/// permitted AND inheritable; that's the caller's responsibility.
#[cfg(target_os = "linux")]
pub fn raise_ambient(amb: &[Capability]) -> Result<()> {
    for cap in amb {
        caps::raise(None, caps::CapSet::Ambient, *cap)
            .map_err(|err| WispError::Capability(format!("raise ambient {cap}: {err}")))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn raise_ambient(_amb: &[Capability]) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_oci_accepts_known_names() {
        let names = vec![
            "CAP_NET_BIND_SERVICE".to_string(),
            "CAP_KILL".to_string(),
            "CAP_SYS_ADMIN".to_string(),
        ];
        let parsed = from_oci(&names).unwrap();
        assert_eq!(
            parsed,
            vec![
                Capability::CAP_NET_BIND_SERVICE,
                Capability::CAP_KILL,
                Capability::CAP_SYS_ADMIN,
            ]
        );
    }

    #[test]
    fn from_oci_rejects_unknown() {
        let names = vec!["CAP_KILL".to_string(), "CAP_NOT_REAL".to_string()];
        let err = from_oci(&names).unwrap_err();
        match err {
            WispError::Capability(msg) => {
                assert!(
                    msg.contains("CAP_NOT_REAL"),
                    "error should name the offending capability, got {msg:?}"
                );
            }
            other => panic!("expected WispError::Capability, got {other:?}"),
        }
    }

    #[test]
    fn from_oci_round_trip_for_all_variants() {
        // Every variant in our `ALL_VARIANTS` list must round-trip
        // through `Display -> from_oci`. If `caps` adds a new variant
        // we'll notice when the new variant doesn't appear in
        // `ALL_VARIANTS` (sanity check below) or when from_oci fails
        // for the formatted name.
        for cap in ALL_VARIANTS {
            let formatted = format!("{cap}");
            let parsed = from_oci(std::slice::from_ref(&formatted)).unwrap_or_else(|err| {
                panic!("variant {cap:?} formatted as {formatted:?} did not round-trip: {err}")
            });
            assert_eq!(parsed, vec![*cap]);
        }

        // Sanity (Linux only): ALL_VARIANTS should match
        // `caps::all()` in size. If caps adds a new variant upstream
        // this fires and forces us to extend ALL_VARIANTS. The Mac
        // stub mirrors caps 0.5.6's variant set by hand so the check
        // would be circular there.
        #[cfg(target_os = "linux")]
        {
            let upstream = caps::all();
            assert_eq!(
                upstream.len(),
                ALL_VARIANTS.len(),
                "caps::all() reports {} variants but ALL_VARIANTS has {}; \
                 extend the local list to keep round-trip coverage exhaustive",
                upstream.len(),
                ALL_VARIANTS.len()
            );
        }
    }

    #[test]
    fn from_oci_empty_input_returns_empty_vec() {
        let parsed = from_oci(&[]).unwrap();
        assert!(parsed.is_empty(), "empty input should yield empty vec");
    }

    /// Compile-only smoke test on Linux: the cfg-gated functions must
    /// at least typecheck. We can't actually call them in unit tests
    /// without root + a fresh process, so we just take their addresses.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_functions_have_expected_signatures() {
        let _drop: fn(&[Capability]) -> Result<()> = drop_bounding;
        let _set_p: fn(&[Capability]) -> Result<()> = set_permitted;
        let _set_e: fn(&[Capability]) -> Result<()> = set_effective;
        let _set_i: fn(&[Capability]) -> Result<()> = set_inheritable;
        let _raise: fn(&[Capability]) -> Result<()> = raise_ambient;
    }
}
