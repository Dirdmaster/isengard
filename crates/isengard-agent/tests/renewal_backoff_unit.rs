use chrono::{Duration, Utc};
use isengard_agent::tls::renewal::should_retry;
use isengard_storage::TlsCertMeta;

fn meta(attempt_count: u32, last_attempt_minutes_ago: i64, error: Option<&str>) -> TlsCertMeta {
    let now = Utc::now();
    let host_id = isengard_storage::HostId::from_bytes([0u8; 16]);
    TlsCertMeta {
        public_hostname: "h.test".into(),
        host_id,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + Duration::days(90),
        last_renewed_at: None,
        next_renewal_at: now,
        serial: None,
        last_attempt_at: Some(now - Duration::minutes(last_attempt_minutes_ago)),
        last_error: error.map(String::from),
        attempt_count,
    }
}

#[test]
fn never_attempted_always_retries() {
    let mut m = meta(0, 0, None);
    m.last_attempt_at = None;
    assert!(should_retry(&m, Utc::now()));
}

#[test]
fn first_failure_retries_after_one_hour() {
    let m = meta(1, 30, Some("rate limited"));
    assert!(!should_retry(&m, Utc::now()), "30 min in is too soon");

    let m = meta(1, 65, Some("rate limited"));
    assert!(
        should_retry(&m, Utc::now()),
        "65 min in is past the 1h backoff"
    );
}

#[test]
fn fifth_failure_uses_24h_backoff() {
    let m = meta(5, 10 * 60, Some("still failing"));
    assert!(
        !should_retry(&m, Utc::now()),
        "10h in is too soon for 5th failure"
    );

    let m = meta(5, 25 * 60, Some("still failing"));
    assert!(
        should_retry(&m, Utc::now()),
        "25h in is past the 24h backoff"
    );
}
