//! `isd ssh trust <host>` + `isd ssh untrust <host>` implementation.
//!
//! Bootstrap of a non-Isengard server: piggyback on the operator's
//! existing SSH access to install `/etc/ssh/isengard_ca.pub` plus a
//! `sshd_config.d/40-isengard-ca.conf` drop-in that points
//! `TrustedUserCAKeys` at it. Reload sshd. Record the host locally so
//! the picker added in PR 4 can show it alongside Isengard hosts.
//!
//! Untrust is local-only: it removes the entry from
//! `~/.config/isd/trusted_hosts.toml` and leaves the remote sshd
//! config alone. Pulling the CA off a server is an operator-managed
//! action (it touches their other workflows, not isd).

// Placeholder so `pub mod trust;` compiles before Task 3.2 wires up
// the actual implementation. The full surface lands in the next
// commit.
