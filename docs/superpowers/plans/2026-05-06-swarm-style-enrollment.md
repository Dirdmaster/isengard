# Swarm-style enrollment join command (plan)

Spec: docs/superpowers/specs/2026-05-06-swarm-style-enrollment-design.md
Branch: chore/polish-enroll
Target: next

## Tasks

### 1. Agent: ISENGARD_CONTROLLER_CA_PEM_BASE64 env var

File: `crates/isengard-agent/src/enroll.rs`.

- Add `pub const CONTROLLER_CA_PEM_BASE64_ENV: &str =
  "ISENGARD_CONTROLLER_CA_PEM_BASE64";`
- Update module docs: list the new env var as resolution step 2 (between
  `_PEM_PATH` and `_PEM`).
- In `build_bootstrap_tls`, after the `_PATH` branch and before the `_PEM`
  branch, decode the base64 var (standard alphabet, padded). On decode
  error, return an error with the env name in the message. On success,
  pass the bytes through `String::from_utf8` (PEM is ASCII) and call
  `pin_ca`.
- Tests in `bootstrap_tls_tests`:
  - `base64_env_is_decoded`: set var to base64 of a stub PEM, assert no
    error.
  - `base64_env_invalid_surfaces_error`: set var to `not-base64!`,
    assert error mentions the env name.
  - Use `temp-env` if available; otherwise scope with a guard that
    unsets after.

Add `base64` to `crates/isengard-agent/Cargo.toml` (workspace dep).

### 2. CLI: rework `controller token mint --role agent`

File: `crates/isengard/src/main.rs`.

- Add `clap` flags on `TokenOp::Mint`:
  - `--public-addr <host:port>` (env `ISENGARD_PUBLIC_ADDR`).
  - `--image <ref>` default `ghcr.io/dirdmaster/isengard:next`.
  - `--format <text|token>` default `text`.
- After minting, when `format == token`, keep the bare-token print
  (current behavior, scripts depend on it).
- When `format == text`:
  - Compute expiry from token TTL (already chrono).
  - Read CA root PEM via `Authority::root_cert_pem()` (already loaded).
  - Base64-encode it (`base64::engine::general_purpose::STANDARD`).
  - Resolve controller URL: `--public-addr` > env > `controller.local:9417`.
  - Print the join block (see spec for exact text).
- The text output is plain ASCII. Use stderr for the leading "Token minted"
  banner so the join command itself can be piped/grepped if needed; or
  put everything on stdout and document that scripts use `--format token`.
  Decision: everything on stdout, the script path is `--format token`.

Add `base64` to `crates/isengard/Cargo.toml`.

### 3. Tests: CLI surface

File: `crates/isengard/tests/plugin_loading.rs`.

- `mint_help_lists_format_flag`: `controller token mint --help` contains
  `--format` and `--public-addr`.
- A round-trip test would need a populated db; skip for this PR. The
  inline format selection is exercised by integration tests on the
  controller service (already present).

### 4. docker/compose.yaml

New file `docker/compose.yaml`:

```yaml
services:
  controller:
    image: ghcr.io/dirdmaster/isengard:next
    container_name: isd-controller
    command: ["controller", "--public-addr", "controller:9417"]
    restart: unless-stopped
    ports:
      - "9417:9417"
      - "8080:8080"
    volumes:
      - controller-state:/var/lib/isengard

  agent:
    image: ghcr.io/dirdmaster/isengard:next
    container_name: isd-agent
    depends_on: [controller]
    restart: unless-stopped
    command:
      - agent
      - --controller=https://controller:9417
      - --state-dir=/var/lib/isengard
    environment:
      ISENGARD_ENROLL_TOKEN: ${ISENGARD_ENROLL_TOKEN}
      ISENGARD_CONTROLLER_CA_PEM_BASE64: ${ISENGARD_CONTROLLER_CA_PEM_BASE64}
    volumes:
      - agent-state:/var/lib/isengard
      - /var/run/docker.sock:/var/run/docker.sock

volumes:
  controller-state:
  agent-state:
```

Note `--public-addr controller:9417` is plumbed via the controller's CLI
so a `mint` invocation inside the controller container outputs the right
URL for compose-network use.

### 5. docker/README.md

New file. Walks through:

1. `docker compose up -d controller`
2. Mint a token: `docker exec isd-controller isengard controller token
   mint --role agent` (output is the join command).
3. For compose: extract the env values from the printed command and
   stuff them in `.env`, then `docker compose up -d agent`.
4. For a remote host: paste the printed `docker run` block on that host.

Mention the bare-token mode for CI and the `ca export` command for
advanced flows.

### 6. README.md

Replace the manual `ca export | mint | docker run` recipe with a single
copy-paste of the join-command output. Keep the structure (controller
first, then agent), just collapse the four-step section.

### 7. RELEASE_NOTES_ENROLLMENT_UX.md

New file. Before / after example, env-var addition, `--format token`
escape hatch, compat statement.

### 8. Phase 14 release notes update

`docs/RELEASE_NOTES_PHASE_14.md`: add a "2026-05-06 update" callout at
the top pointing at the new join-command flow as the recommended path.
Keep the existing manual recipe below as the advanced fallback.

## Verification

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
```

Manual smoke:

```sh
# in the worktree:
cargo run --bin isengard -- controller --state-dir /tmp/iso-test &
sleep 1
cargo run --bin isengard -- controller --state-dir /tmp/iso-test \
  token mint --role agent --public-addr myhost.example.com:9417
# expect: full join block, token + base64 inlined.
cargo run --bin isengard -- controller --state-dir /tmp/iso-test \
  token mint --role agent --format token
# expect: bare token (legacy compat).
```

## Risks

- `Authority::load_or_init` is invoked twice in mint mode (once by the
  enrollment service, once for the PEM). It's a no-op on second call so
  the impact is negligible, but worth a comment.
- The `controller.local` default is wrong for many deployments. The
  expiry footer reminds operators to mint a fresh token, but the URL is
  baked in at mint time. We rely on `--public-addr` for cross-host.
- Backslash-continuation join commands paste cleanly into bash/zsh/fish
  but break on Windows cmd.exe. Acceptable: agents run on Linux.

## Out of scope

- JSON output format (follow-up).
- Auto-detecting public addr via DNS / mDNS.
- Replacing `ca export` (it stays).
