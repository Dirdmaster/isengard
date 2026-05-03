use isengard_agent::tls::challenge_state::ChallengeState;
use std::sync::Arc;

#[tokio::test]
async fn install_then_lookup_returns_key_authorization() {
    let st = Arc::new(ChallengeState::new());
    st.install("token-abc", "key-auth-xyz").await;
    assert_eq!(
        st.lookup("token-abc").await.as_deref(),
        Some("key-auth-xyz")
    );
}

#[tokio::test]
async fn remove_clears_token() {
    let st = Arc::new(ChallengeState::new());
    st.install("t", "ka").await;
    st.remove("t").await;
    assert!(st.lookup("t").await.is_none());
}

#[tokio::test]
async fn lookup_unknown_returns_none() {
    let st = Arc::new(ChallengeState::new());
    assert!(st.lookup("nope").await.is_none());
}
