use isengard_manifest::{
    FleetManifest, HookErrorPolicy, HookEvent, ManifestError, StackManifest, Strategy,
};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from("/tmp/test-root")
}

#[test]
fn minimal_manifest_parses() {
    let text = r#"
        name = "hello"
        compose = ["compose.toml"]
    "#;
    let m = StackManifest::from_str(text, root()).unwrap();
    assert_eq!(m.name, "hello");
    assert_eq!(m.compose, vec![PathBuf::from("compose.toml")]);
    assert_eq!(m.strategy, Strategy::Auto);
    assert!(m.secrets.is_empty());
    assert!(m.hooks.is_empty());
}

#[test]
fn full_manifest_parses() {
    let text = r#"
        name = "servarr"
        fleet = "homelab"
        compose = ["compose.toml"]
        strategy = "blue-green"
        secrets = ["SONARR_API_KEY", "PLEX_TOKEN"]

        [overlays.staging]
        compose = ["compose.staging.toml"]

        [[hooks]]
        on = "pre-deploy"
        cmd = ["./scripts/backup.sh"]
        timeout = "120s"

        [[hooks]]
        on = "post-deploy"
        cmd = ["./scripts/notify.sh", "deployed"]
        on_error = "continue"
    "#;
    let m = StackManifest::from_str(text, root()).unwrap();
    assert_eq!(m.name, "servarr");
    assert_eq!(m.fleet.as_deref(), Some("homelab"));
    assert_eq!(m.strategy, Strategy::BlueGreen);
    assert_eq!(m.secrets.len(), 2);
    assert_eq!(m.hooks.len(), 2);
    assert_eq!(m.hooks[0].on, HookEvent::PreDeploy);
    assert_eq!(m.hooks[0].timeout, Duration::from_secs(120));
    assert_eq!(m.hooks[0].on_error, HookErrorPolicy::Abort);
    assert_eq!(m.hooks[1].on_error, HookErrorPolicy::Continue);
    assert!(m.overlays.contains_key("staging"));
}

#[test]
fn missing_name_errors() {
    let text = r#"compose = ["compose.toml"]"#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::MissingField("name")));
}

#[test]
fn missing_compose_errors() {
    let text = r#"name = "x""#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::MissingField("compose")));
}

#[test]
fn empty_compose_errors() {
    let text = r#"
        name = "x"
        compose = []
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::EmptyCompose));
}

#[test]
fn unknown_strategy_errors() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        strategy = "yolo"
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::UnknownStrategy(_)));
}

#[test]
fn unknown_hook_event_errors() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        [[hooks]]
        on = "halfway"
        cmd = ["foo"]
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::UnknownHookEvent(_)));
}

#[test]
fn empty_hook_cmd_errors() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        [[hooks]]
        on = "pre-deploy"
        cmd = []
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::EmptyHookCmd));
}

#[test]
fn hook_timeout_accepts_s_and_ms() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        [[hooks]]
        on = "pre-deploy"
        cmd = ["true"]
        timeout = "120s"
        [[hooks]]
        on = "post-deploy"
        cmd = ["true"]
        timeout = "60000ms"
    "#;
    let m = StackManifest::from_str(text, root()).unwrap();
    assert_eq!(m.hooks[0].timeout, Duration::from_secs(120));
    assert_eq!(m.hooks[1].timeout, Duration::from_secs(60));
}

#[test]
fn hook_timeout_defaults_to_60s() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        [[hooks]]
        on = "failure"
        cmd = ["true"]
    "#;
    let m = StackManifest::from_str(text, root()).unwrap();
    assert_eq!(m.hooks[0].timeout, Duration::from_secs(60));
}

#[test]
fn absolute_compose_path_rejected() {
    let text = r#"
        name = "x"
        compose = ["/etc/compose.toml"]
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::AbsoluteComposePath { .. }));
}

#[test]
fn resolved_compose_paths_with_overlay() {
    let text = r#"
        name = "x"
        compose = ["compose.toml"]
        [overlays.staging]
        compose = ["compose.staging.toml", "compose.local.toml"]
    "#;
    let m = StackManifest::from_str(text, PathBuf::from("/srv/x")).unwrap();
    let base = m.resolved_compose_paths(None).unwrap();
    assert_eq!(base, vec![PathBuf::from("/srv/x/compose.toml")]);
    let with_overlay = m.resolved_compose_paths(Some("staging")).unwrap();
    assert_eq!(
        with_overlay,
        vec![
            PathBuf::from("/srv/x/compose.toml"),
            PathBuf::from("/srv/x/compose.staging.toml"),
            PathBuf::from("/srv/x/compose.local.toml"),
        ]
    );
}

#[test]
fn unknown_overlay_errors() {
    let text = r#"
        name = "x"
        compose = ["compose.toml"]
    "#;
    let m = StackManifest::from_str(text, root()).unwrap();
    let err = m.resolved_compose_paths(Some("nope")).unwrap_err();
    assert!(matches!(err, ManifestError::UnknownOverlay(_)));
}

#[test]
fn fleet_manifest_walk_finds_at_root() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("isengard.toml"), "fleet = \"homelab\"\n").unwrap();
    let subdir = repo.join("a").join("b");
    std::fs::create_dir_all(&subdir).unwrap();
    let found = FleetManifest::load_from_walk(&subdir).unwrap();
    assert_eq!(
        found.unwrap(),
        FleetManifest {
            fleet: Some("homelab".into()),
            context: None,
        }
    );
}

#[test]
fn fleet_manifest_walk_stops_at_git_root() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    // No isengard.toml inside the repo. A SIBLING isengard.toml outside
    // the .git boundary must NOT be returned.
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let subdir = repo.join("a");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(tmp.path().parent().unwrap().join("isengard.toml"), "").ok();
    let found = FleetManifest::load_from_walk(&subdir).unwrap();
    assert!(found.is_none(), "walk must stop at .git boundary");
}

#[test]
fn fleet_manifest_walk_returns_none_when_absent() {
    // Plant a .git marker as the boundary so the walk doesn't escape
    // into the host filesystem (which may contain unrelated
    // isengard.toml files for other projects).
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let subdir = tmp.path().join("nested");
    std::fs::create_dir_all(&subdir).unwrap();
    let found = FleetManifest::load_from_walk(&subdir).unwrap();
    assert!(found.is_none());
}

#[test]
fn stack_manifest_load_reads_from_disk() {
    let tmp = TempDir::new().unwrap();
    let manifest_path = tmp.path().join("stack.toml");
    std::fs::write(&manifest_path, "name = \"disk\"\ncompose = [\"c.toml\"]\n").unwrap();
    let m = StackManifest::load(&manifest_path).unwrap();
    assert_eq!(m.name, "disk");
    assert_eq!(m.root, tmp.path());
}

#[test]
fn unknown_on_error_policy_rejected() {
    let text = r#"
        name = "x"
        compose = ["c.toml"]
        [[hooks]]
        on = "pre-deploy"
        cmd = ["true"]
        on_error = "panic"
    "#;
    let err = StackManifest::from_str(text, root()).unwrap_err();
    assert!(matches!(err, ManifestError::UnknownHookErrorPolicy(_)));
}
