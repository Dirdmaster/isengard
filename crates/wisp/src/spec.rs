//! OCI runtime spec loading and validation helpers.
//!
//! Wraps `oci_spec::runtime::Spec` with a `load_and_validate` entry point that
//! enforces the wisp 0.1 invariants: a usable rootfs, a non-empty process
//! `args`, and the five namespaces we require (PID, mount, UTS, IPC, network).
//! User and cgroup namespaces are explicitly rejected because rootless mode
//! and cgroup namespacing are deferred to wisp 0.2.

use std::path::Path;

use oci_spec::runtime::LinuxNamespaceType;

use crate::error::{Result, WispError};

/// Re-export of the OCI runtime spec root type.
pub use oci_spec::runtime::Spec;

/// Five namespaces every wisp 0.1 container must declare.
///
/// `pivot_root` and the device-mount sequence assume a private mount namespace,
/// the entrypoint runs as PID 1 in a fresh PID namespace, the spec hostname is
/// applied through a UTS namespace, IPC objects are isolated, and the
/// container has its own network namespace (even though wisp 0.1 doesn't wire
/// veth pairs yet).
pub const REQUIRED_NAMESPACES: &[LinuxNamespaceType] = &[
    LinuxNamespaceType::Pid,
    LinuxNamespaceType::Mount,
    LinuxNamespaceType::Uts,
    LinuxNamespaceType::Ipc,
    LinuxNamespaceType::Network,
];

/// Load `<bundle>/config.json` and validate it against the wisp 0.1 invariants.
///
/// The validation rules are deliberately narrow: we only reject configurations
/// we know we can't honour. Anything we can ignore safely (annotations,
/// hooks, seccomp profile, and so on) is left for the lifecycle code to
/// handle later.
pub fn load_and_validate(bundle: &Path) -> Result<Spec> {
    let config_path = bundle.join("config.json");
    let spec = Spec::load(&config_path)?;

    validate(&spec, bundle)?;
    Ok(spec)
}

fn validate(spec: &Spec, bundle: &Path) -> Result<()> {
    // process.args: must exist and be non-empty.
    let process = spec.process().as_ref().ok_or_else(|| {
        WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.process is required".to_string(),
        ))
    })?;
    let args = process.args().as_ref().ok_or_else(|| {
        WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.process.args is required".to_string(),
        ))
    })?;
    if args.is_empty() {
        return Err(WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.process.args must be non-empty".to_string(),
        )));
    }

    // root.path: must point at a directory that exists relative to the bundle.
    let root = spec.root().as_ref().ok_or_else(|| {
        WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.root is required".to_string(),
        ))
    })?;
    let rootfs = if root.path().is_absolute() {
        root.path().clone()
    } else {
        bundle.join(root.path())
    };
    if !rootfs.exists() {
        return Err(WispError::Spec(oci_spec::OciSpecError::Other(format!(
            "rootfs path does not exist: {}",
            rootfs.display()
        ))));
    }

    // linux.namespaces: must contain the five required, must NOT contain
    // user / cgroup (deferred to wisp 0.2).
    let linux = spec.linux().as_ref().ok_or_else(|| {
        WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.linux is required".to_string(),
        ))
    })?;
    let namespaces = linux.namespaces().as_ref().ok_or_else(|| {
        WispError::Spec(oci_spec::OciSpecError::Other(
            "spec.linux.namespaces is required".to_string(),
        ))
    })?;

    let declared: Vec<LinuxNamespaceType> = namespaces.iter().map(|n| n.typ()).collect();

    for required in REQUIRED_NAMESPACES {
        if !declared.contains(required) {
            return Err(WispError::Spec(oci_spec::OciSpecError::Other(format!(
                "required namespace missing: {required:?}"
            ))));
        }
    }

    for ns in &declared {
        match ns {
            LinuxNamespaceType::User => {
                return Err(WispError::Spec(oci_spec::OciSpecError::Other(
                    "user namespace declared: deferred to wisp 0.2".to_string(),
                )));
            }
            LinuxNamespaceType::Cgroup => {
                return Err(WispError::Spec(oci_spec::OciSpecError::Other(
                    "cgroup namespace declared: deferred to wisp 0.2".to_string(),
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;

    use oci_spec::runtime::{
        LinuxBuilder, LinuxNamespaceBuilder, ProcessBuilder, RootBuilder, SpecBuilder,
    };
    use tempfile::TempDir;

    /// Build a bundle on disk: `<dir>/config.json` plus an empty `<dir>/rootfs/`.
    /// Returns the bundle root.
    fn write_bundle(spec: &Spec) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("rootfs")).expect("rootfs dir");
        spec.save(dir.path().join("config.json"))
            .expect("save spec");
        dir
    }

    fn ns(typ: LinuxNamespaceType) -> oci_spec::runtime::LinuxNamespace {
        LinuxNamespaceBuilder::default().typ(typ).build().unwrap()
    }

    fn five_namespaces() -> Vec<oci_spec::runtime::LinuxNamespace> {
        vec![
            ns(LinuxNamespaceType::Pid),
            ns(LinuxNamespaceType::Mount),
            ns(LinuxNamespaceType::Uts),
            ns(LinuxNamespaceType::Ipc),
            ns(LinuxNamespaceType::Network),
        ]
    }

    fn minimal_spec(args: Vec<String>, namespaces: Vec<oci_spec::runtime::LinuxNamespace>) -> Spec {
        let process = ProcessBuilder::default()
            .args(args)
            .cwd(PathBuf::from("/"))
            .build()
            .unwrap();
        let root = RootBuilder::default()
            .path(PathBuf::from("rootfs"))
            .build()
            .unwrap();
        let linux = LinuxBuilder::default()
            .namespaces(namespaces)
            .build()
            .unwrap();
        SpecBuilder::default()
            .process(process)
            .root(root)
            .linux(linux)
            .build()
            .unwrap()
    }

    #[test]
    fn valid_minimal_config_passes() {
        let spec = minimal_spec(
            vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
            five_namespaces(),
        );
        let bundle = write_bundle(&spec);

        let loaded = load_and_validate(bundle.path()).expect("should load");
        assert_eq!(
            loaded.process().as_ref().unwrap().args().as_ref().unwrap()[0],
            "/bin/sh"
        );
    }

    #[test]
    fn missing_process_args_rejected() {
        // Build a spec with an explicitly empty args vec. The OCI spec marks
        // args optional, so we have to craft this through SpecBuilder (an
        // empty Vec<String> serialises out cleanly).
        let process = ProcessBuilder::default()
            .args(Vec::<String>::new())
            .cwd(PathBuf::from("/"))
            .build()
            .unwrap();
        let root = RootBuilder::default()
            .path(PathBuf::from("rootfs"))
            .build()
            .unwrap();
        let linux = LinuxBuilder::default()
            .namespaces(five_namespaces())
            .build()
            .unwrap();
        let spec = SpecBuilder::default()
            .process(process)
            .root(root)
            .linux(linux)
            .build()
            .unwrap();
        let bundle = write_bundle(&spec);

        let err = load_and_validate(bundle.path()).expect_err("should reject empty args");
        let msg = err.to_string();
        assert!(
            msg.contains("args"),
            "expected args-related error, got: {msg}"
        );
    }

    #[test]
    fn missing_rootfs_rejected() {
        let spec = minimal_spec(vec!["/bin/sh".to_string()], five_namespaces());
        let dir = tempfile::tempdir().expect("tempdir");
        // Note: deliberately do NOT create rootfs/ subdir.
        spec.save(dir.path().join("config.json"))
            .expect("save spec");

        let err = load_and_validate(dir.path()).expect_err("should reject missing rootfs");
        let msg = err.to_string();
        assert!(msg.contains("rootfs"), "expected rootfs error, got: {msg}");
    }

    #[test]
    fn user_namespace_rejected_as_deferred() {
        let mut nss = five_namespaces();
        nss.push(ns(LinuxNamespaceType::User));
        let spec = minimal_spec(vec!["/bin/sh".to_string()], nss);
        let bundle = write_bundle(&spec);

        let err = load_and_validate(bundle.path()).expect_err("should reject user ns");
        let msg = err.to_string();
        assert!(
            msg.contains("user namespace") && msg.contains("0.2"),
            "expected deferred-to-0.2 user-ns error, got: {msg}"
        );
    }

    #[test]
    fn cgroup_namespace_rejected_as_deferred() {
        let mut nss = five_namespaces();
        nss.push(ns(LinuxNamespaceType::Cgroup));
        let spec = minimal_spec(vec!["/bin/sh".to_string()], nss);
        let bundle = write_bundle(&spec);

        let err = load_and_validate(bundle.path()).expect_err("should reject cgroup ns");
        let msg = err.to_string();
        assert!(
            msg.contains("cgroup namespace") && msg.contains("0.2"),
            "expected deferred-to-0.2 cgroup-ns error, got: {msg}"
        );
    }

    #[test]
    fn missing_required_namespace_rejected() {
        // Drop the network namespace; the validator should refuse.
        let nss = vec![
            ns(LinuxNamespaceType::Pid),
            ns(LinuxNamespaceType::Mount),
            ns(LinuxNamespaceType::Uts),
            ns(LinuxNamespaceType::Ipc),
        ];
        let spec = minimal_spec(vec!["/bin/sh".to_string()], nss);
        let bundle = write_bundle(&spec);

        let err = load_and_validate(bundle.path()).expect_err("should reject missing ns");
        let msg = err.to_string();
        assert!(
            msg.contains("required namespace missing"),
            "expected missing-ns error, got: {msg}"
        );
    }
}
