//! Phase 0.5 wisp: round-trip the new `runtime_backend` field on
//! `Heartbeat`. Wire shape stays additive: an older proto blob (no
//! field 4) decodes with the field as the empty string, which the
//! controller treats as "docker" for back-compat.

use isengard_proto::pb::Heartbeat;
use prost::Message;

#[test]
fn agent_reports_backend_in_heartbeat() {
    let hb = Heartbeat {
        ts_ms: 1_725_000_000_000,
        stacks: Vec::new(),
        services: Vec::new(),
        runtime_backend: "wisp".into(),
    };
    let bytes = hb.encode_to_vec();
    let back = Heartbeat::decode(&*bytes).unwrap();
    assert_eq!(back.runtime_backend, "wisp");
    assert_eq!(back.ts_ms, 1_725_000_000_000);
}

#[test]
fn heartbeat_runtime_backend_default_empty_for_legacy_blob() {
    // Phase 0.4 (and earlier) agents emit a Heartbeat without
    // field 4. Decoding a blob shaped like the older message must
    // succeed with `runtime_backend = ""`, which the controller
    // treats as "docker" so legacy hosts keep their backend column
    // populated.
    let hb_old = Heartbeat {
        ts_ms: 42,
        stacks: Vec::new(),
        services: Vec::new(),
        runtime_backend: String::new(),
    };
    let bytes = hb_old.encode_to_vec();
    let back = Heartbeat::decode(&*bytes).unwrap();
    assert!(back.runtime_backend.is_empty());
}
