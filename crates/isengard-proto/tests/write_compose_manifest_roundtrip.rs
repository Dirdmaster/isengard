//! Phase 0.13: round-trip the new optional fields on `WriteCompose`
//! (`manifest_toml`, `secrets`, `hooks`, `deployment_id`) plus the new
//! `LifecycleHook` message. Older agents that don't know about field
//! numbers 6..=9 decode them as zero values / empty repeated lists,
//! preserving wire compatibility.

use isengard_proto::pb::{LifecycleHook, WriteCompose};
use prost::Message;

#[test]
fn write_compose_carries_manifest_fields() {
    let msg = WriteCompose {
        request_id: "req-1".into(),
        stack_name: "servarr".into(),
        compose_yaml: "services:\n  sonarr:\n    image: linuxserver/sonarr\n".into(),
        expected_sha256: "abcd".into(),
        force: false,
        manifest_toml: "name = \"servarr\"\ncompose = [\"compose.toml\"]\n".into(),
        secrets: vec!["SONARR_API_KEY".into(), "PROWLARR_API_KEY".into()],
        hooks: vec![LifecycleHook {
            on: "pre-deploy".into(),
            cmd: vec!["./scripts/backup.sh".into()],
            timeout_ms: 120_000,
            on_error: "abort".into(),
        }],
        deployment_id: "01JJ0".into(),
    };
    let bytes = msg.encode_to_vec();
    let back = WriteCompose::decode(&*bytes).unwrap();
    assert_eq!(back.manifest_toml, msg.manifest_toml);
    assert_eq!(back.secrets, msg.secrets);
    assert_eq!(back.hooks.len(), 1);
    assert_eq!(back.hooks[0].on, "pre-deploy");
    assert_eq!(back.hooks[0].timeout_ms, 120_000);
    assert_eq!(back.deployment_id, "01JJ0");
}

#[test]
fn write_compose_legacy_blob_decodes_with_empty_phase_0_13_fields() {
    // Pre-0.13 controllers/agents don't set fields 6..=9. Round-trip a
    // message that leaves those fields empty and verify nothing breaks.
    let msg = WriteCompose {
        request_id: "req-1".into(),
        stack_name: "legacy".into(),
        compose_yaml: "services:\n  web: { image: nginx }\n".into(),
        expected_sha256: String::new(),
        force: true,
        manifest_toml: String::new(),
        secrets: Vec::new(),
        hooks: Vec::new(),
        deployment_id: String::new(),
    };
    let bytes = msg.encode_to_vec();
    let back = WriteCompose::decode(&*bytes).unwrap();
    assert!(back.manifest_toml.is_empty());
    assert!(back.secrets.is_empty());
    assert!(back.hooks.is_empty());
    assert!(back.deployment_id.is_empty());
}
