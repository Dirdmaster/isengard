use chrono::Duration;
use isengard_agent::cert_renewal::should_renew;

#[test]
fn does_not_renew_well_before_50pct() {
    let issued = chrono::Utc::now() - Duration::days(5);
    let expires = chrono::Utc::now() + Duration::days(25);
    assert!(!should_renew(issued, expires));
}

#[test]
fn renews_at_or_past_50pct() {
    let issued = chrono::Utc::now() - Duration::days(16);
    let expires = chrono::Utc::now() + Duration::days(14);
    assert!(should_renew(issued, expires));
}
