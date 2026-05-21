//! Isengard's SSH user-certificate authority.
//!
//! Sibling primitive to the TLS [`Authority`](crate::ca::Authority). The
//! controller holds an ed25519 keypair, signs short-lived OpenSSH user
//! certificates, and every fleet host trusts the corresponding public key
//! via a `TrustedUserCAKeys` directive installed by the agent at enroll
//! time.
//!
//! The private key persists in the encrypted secrets store under the name
//! `ssh_ca_private_key`. Same envelope as `master.key`: ChaCha20-Poly1305
//! with the AAD bound to the row name, ciphertext stored in SQLite. The
//! cleartext OpenSSH PEM only lives inside the controller process; logs
//! refer to the row by name.
//!
//! # Surface
//!
//! - [`SshAuthority::load_or_init`]: boot-time load or fresh-mint. Idempotent.
//! - [`SshAuthority::public_key_openssh`]: pubkey bytes agents drop into
//!   `/etc/isengard/ssh_ca.pub`.
//! - [`SshAuthority::sign_user_cert`]: mint a short-lived user cert for the
//!   given operator pubkey.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use ssh_key::certificate::{Builder, CertType};
use ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};

use crate::secrets::{SecretsError, SecretsStore};

/// Secrets-store row name for the SSH CA private key. Same row through the
/// life of the install; rotations replace the value in place.
const SSH_CA_SECRET_NAME: &str = "ssh_ca_private_key";

/// Label recorded against the secret row when the controller mints a fresh
/// CA at boot. Operator-facing tooling shows this as the row's creator.
const SSH_CA_CREATED_BY: &str = "controller-bootstrap";

/// SSH user-certificate authority.
///
/// Holds the ed25519 keypair and the cached OpenSSH-wire-format pubkey
/// bytes. Cheap to share via `Arc`; the signing path is `&self` and does
/// not mutate.
pub struct SshAuthority {
    /// Ed25519 signing key in `ssh-key` crate form. Loaded from the
    /// secrets store at boot via `load_or_init`; never serialized back
    /// to disk after that.
    private_key: PrivateKey,
    /// Cached OpenSSH-wire-format pubkey bytes. Pre-computed at boot so
    /// the `GetSshCa` RPC and the `isd ssh ca pubkey` CLI both serve
    /// without re-encoding on every request.
    public_key_openssh: Vec<u8>,
}

impl SshAuthority {
    /// Load the SSH CA keypair from the secrets store, or mint and persist
    /// a fresh ed25519 keypair when no row exists.
    ///
    /// Called once at controller boot, after the secrets store has been
    /// unlocked. Idempotent: a second call returns an authority backed by
    /// the same persisted bytes.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the secrets store is locked, when the persisted
    /// row exists but fails to parse as an OpenSSH private key, or when
    /// minting a fresh keypair fails.
    pub async fn load_or_init(secrets: &SecretsStore) -> Result<Self> {
        match secrets.fetch(SSH_CA_SECRET_NAME).await {
            Ok(bytes) => {
                let pem =
                    std::str::from_utf8(&bytes).context("ssh ca private key is not valid utf-8")?;
                let private_key =
                    PrivateKey::from_openssh(pem).context("parse stored ssh ca private key")?;
                let public_key_openssh = private_key
                    .public_key()
                    .to_openssh()
                    .context("serialize ssh ca public key")?
                    .into_bytes();
                Ok(Self {
                    private_key,
                    public_key_openssh,
                })
            }
            Err(SecretsError::NotFound(_)) => Self::mint_and_persist(secrets).await,
            Err(e) => Err(anyhow::Error::new(e).context("fetch ssh ca private key")),
        }
    }

    /// Mint a fresh ed25519 keypair, persist the private side as PEM in
    /// the secrets store under `SSH_CA_SECRET_NAME`, and return an
    /// authority backed by it. Called from `load_or_init` on the first
    /// boot when no SSH CA row exists yet.
    async fn mint_and_persist(secrets: &SecretsStore) -> Result<Self> {
        let private_key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
            .context("mint ssh ca private key")?;
        let pem = private_key
            .to_openssh(LineEnding::LF)
            .context("serialize ssh ca private key")?;
        secrets
            .put(SSH_CA_SECRET_NAME, pem.as_bytes(), Some(SSH_CA_CREATED_BY))
            .await
            .context("persist ssh ca private key")?;
        let public_key_openssh = private_key
            .public_key()
            .to_openssh()
            .context("serialize ssh ca public key")?
            .into_bytes();
        Ok(Self {
            private_key,
            public_key_openssh,
        })
    }

    /// OpenSSH-wire-format public key bytes.
    ///
    /// Agents fetch these once at enrollment and drop them into
    /// `/etc/isengard/ssh_ca.pub`. The format is the standard
    /// `ssh-ed25519 AAAA...` single-line representation `sshd` consumes
    /// via `TrustedUserCAKeys`.
    pub fn public_key_openssh(&self) -> &[u8] {
        &self.public_key_openssh
    }

    /// Sign a short-lived user certificate.
    ///
    /// Mints an OpenSSH user cert for `target`, valid from now for `ttl`,
    /// authorizing the given POSIX `principals`. The `comment` lands in
    /// the cert's `key_id` field, which `ssh -v` and the audit journal
    /// surface for operator identification.
    ///
    /// The cert grants `permit-pty` so interactive shells work; no other
    /// extensions or critical options are set. Stricter caps land in a
    /// later phase.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the current system clock predates the UNIX
    /// epoch, when the cert builder rejects a principal name, or when
    /// signing fails.
    pub fn sign_user_cert(
        &self,
        target: &PublicKey,
        principals: &[String],
        ttl: Duration,
        comment: &str,
    ) -> Result<Vec<u8>> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("system clock predates unix epoch")?
            .as_secs();
        let valid_after = now;
        let valid_before = now
            .checked_add(ttl.as_secs())
            .context("ttl overflows unix seconds")?;

        let mut builder = Builder::new_with_random_nonce(
            &mut ssh_key::rand_core::OsRng,
            target.key_data().clone(),
            valid_after,
            valid_before,
        )
        .context("init ssh cert builder")?;
        builder.cert_type(CertType::User).context("set cert type")?;
        for principal in principals {
            builder
                .valid_principal(principal)
                .with_context(|| format!("add principal {principal:?}"))?;
        }
        builder.key_id(comment).context("set cert key id")?;
        // Standard interactive shell access. Stricter caps (no-port-forward,
        // no-agent-forward, force-command) belong in a later phase wired to
        // per-host labels.
        builder
            .extension("permit-pty", "")
            .context("set permit-pty extension")?;

        let cert = builder.sign(&self.private_key).context("sign ssh cert")?;
        let openssh = cert.to_openssh().context("serialize ssh cert")?;
        Ok(openssh.into_bytes())
    }

    /// Mint a fresh in-memory SSH CA without touching the secrets store.
    ///
    /// Tests + downstream fixtures that exercise unrelated controller
    /// surfaces use this to assemble a `ControllerHandles` without
    /// standing up an unlocked secrets store. The keypair lives only
    /// in the returned value; nothing persists.
    #[doc(hidden)]
    pub fn for_tests() -> Result<Self> {
        let private_key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
            .context("mint ssh ca private key for tests")?;
        let public_key_openssh = private_key
            .public_key()
            .to_openssh()
            .context("serialize ssh ca public key for tests")?
            .into_bytes();
        Ok(Self {
            private_key,
            public_key_openssh,
        })
    }
}
