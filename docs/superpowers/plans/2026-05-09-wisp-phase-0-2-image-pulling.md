# Wisp Phase 0.2 Image Pulling: Implementation Plan

> Spec: [`2026-05-09-wisp-phase-0-2-image-pulling-design.md`](../specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md). Branch: `wisp/phase-0-1` (continuing the foundation arc; will rename / fork the branch later if review requires).

## Scope

Land `crates/wisp-image/` (library) so wisp can pull OCI images from public registries, cache them by digest, extract layered tarballs (with whiteouts), and synthesise a runnable bundle via `BundleBuilder`. Ship a `wisp image pull|list|rm|gc` subcommand surface and a `wisp run --image <ref>` flag that goes from registry to running container in one step.

Out of scope: image building, image pushing, private registries / auth, image signing, overlay storage driver, cross-arch emulation. Per spec.

## Dev environment

Same OrbStack VM (`wisp`) as Phase 0.1. Mac home is bind-mounted; edits land in the VM with no sync. Pulls require network, which the VM has by default. Linux + arm64 native; the demo image is `docker.io/library/alpine:3.19` (multi-arch index, picks the arm64 manifest entry).

Loop: edit on Mac, then
```
orb -m wisp -u root bash -c 'PATH=/home/dirdmaster/.cargo/bin:$PATH; \
  cd /Users/dirdmaster/Projects/isengard/.worktrees/next && cargo test -p wisp-image'
```

## Files touched

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | add `crates/wisp-image` to `members` |
| `crates/wisp-image/Cargo.toml` | new, deps: `wisp` (path), `reqwest` (blocking, rustls-tls), `flate2`, `zstd`, `tar`, `sha2`, `hex`, `oci-spec`, `serde`, `serde_json`, `thiserror`, `tracing`, `fs2` (file-locking), `url` |
| `crates/wisp-image/src/lib.rs` | new, public API surface |
| `crates/wisp-image/src/error.rs` | new, `WispImageError` |
| `crates/wisp-image/src/reference.rs` | new, ImageRef + parse |
| `crates/wisp-image/src/registry/mod.rs` | new, public client surface |
| `crates/wisp-image/src/registry/auth.rs` | new, /v2/ challenge + bearer token |
| `crates/wisp-image/src/registry/manifest.rs` | new, manifest + index deserialisation |
| `crates/wisp-image/src/registry/blob.rs` | new, blob fetch + digest verify |
| `crates/wisp-image/src/store/mod.rs` | new, ContentStore CAS layout |
| `crates/wisp-image/src/store/layer.rs` | new, layer extraction with whiteouts |
| `crates/wisp-image/src/store/meta.rs` | new, image -> manifest -> layer index |
| `crates/wisp-image/src/bundle.rs` | new, BundleBuilder |
| `crates/wisp-image/examples/pull-alpine.rs` | new, `cargo run --example pull-alpine` |
| `crates/wisp-image/tests/whiteout_handling.rs` | new, hermetic |
| `crates/wisp-image/tests/gc_drops_unreferenced.rs` | new, hermetic |
| `crates/wisp-image/tests/pull_real_image.rs` | new, `#[ignore]` (needs network) |
| `crates/wisp-image/tests/roundtrip_run.rs` | new, `#[ignore]` (needs network + root) |
| `crates/wisp-cli/Cargo.toml` | add `wisp-image` |
| `crates/wisp-cli/src/main.rs` | add `image` subcommand group + `--image` flag on `run` |
| `crates/wisp-image/README.md` | new, dev-loop quickstart |
| `docs/RELEASE_NOTES_WISP_PHASE_0_2.md` | new |

## Steps

Five sequenced dispatches, each ending in commits. Branch is `wisp/phase-0-1` (Phase 0.2 work continues on the same branch since 0.1 isn't merged yet). All commits land local; do NOT push. Per `feedback_implementer_opus`: Opus implementers; Sonnet code-reviewer at the syscall + network boundaries.

### Dispatch A: crate skeleton + reference parsing + content store

Pure Rust, no network, no Linux syscalls. Three commits.

#### Step A1: workspace addition + crate skeleton

- Add `crates/wisp-image` to the workspace `Cargo.toml` `members`.
- Minimal `crates/wisp-image/Cargo.toml` with `wisp` path dep + bare deps for the next steps (`thiserror`, `tracing`, `serde`, `serde_json`, `oci-spec`, `sha2`, `hex`, `url`).
- `crates/wisp-image/src/lib.rs`: `pub mod error; pub use error::WispImageError;` plus a doc comment.
- `crates/wisp-image/src/error.rs`: `WispImageError` enum stub with `Io`, `Parse(String)`, `Json`, `NotFound`. Add variants as later steps need them.
- Validate: `cargo build -p wisp-image`, `cargo test -p wisp-image` (zero tests).
- Commit: `feat(wisp-image): workspace skeleton`

#### Step A2: reference parsing

- `crates/wisp-image/src/reference.rs`: `pub struct ImageRef { registry, repo, tag, digest }`, `pub fn parse(s: &str) -> Result<ImageRef, WispImageError>`. Defaults: registry `docker.io`, namespace `library` for single-component names, tag `latest`. Tag and digest are mutually exclusive.
- `Display` impl that round-trips. Tests:
  - `parse_bare_name` -> docker.io/library/alpine:latest
  - `parse_name_with_tag` -> docker.io/library/nginx:alpine
  - `parse_namespaced` -> docker.io/foo/bar:baz
  - `parse_explicit_registry` -> ghcr.io/dirdmaster/foo:bar
  - `parse_digest_pull` -> docker.io/library/alpine@sha256:abc...
  - `parse_rejects_empty / multiple_at / multiple_colons / digest_with_tag`
  - `display_round_trips` for all valid forms above
- Validate: `cargo test -p wisp-image`.
- Commit: `feat(wisp-image): ImageRef parsing`

#### Step A3: content store

- `crates/wisp-image/src/store/mod.rs`: `pub struct ContentStore { root: PathBuf }`. Methods:
  - `pub fn new(root: &Path) -> Result<Self>` (mkdir root, blobs/sha256, index, refs subdirs)
  - `pub fn blob_path(&self, digest: &str) -> PathBuf`
  - `pub fn write_blob(&self, content: &[u8]) -> Result<String>` (computes sha256, atomic write to blobs/sha256/<hex>, returns "sha256:<hex>")
  - `pub fn write_blob_streaming<R: Read>(&self, reader: R, expected: Option<&str>) -> Result<String>` (sha256 hashes while streaming; verifies against expected if provided)
  - `pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>>`
  - `pub fn open_blob(&self, digest: &str) -> Result<File>`
  - `pub fn has_blob(&self, digest: &str) -> bool`
  - `pub fn put_tag(&self, registry, repo, tag, manifest_digest)` (atomic write to index/<reg>/<repo>/tag/<tag>)
  - `pub fn lookup_tag(&self, registry, repo, tag) -> Option<String>`
  - `pub fn list_images(&self) -> Result<Vec<(ImageRef, String)>>` (walks index, returns ref + manifest digest pairs)
  - `pub fn add_ref(&self, bundle_id, layer_digests: &[String]) -> Result<()>` (writes refs/<bundle-id>/layers)
  - `pub fn drop_ref(&self, bundle_id) -> Result<()>` (removes refs/<bundle-id>/)
  - `pub fn referenced_layers(&self) -> Result<HashSet<String>>` (union of all refs/<bundle-id>/layers)
  - `pub fn gc(&self) -> Result<GcReport>` (walks blobs/sha256, drops any not in referenced_layers + not a manifest pointed at by index)
- File-locking: hold an `fs2::FileExt::lock_exclusive` on a `<root>/.lock` during writes. Reads don't lock.
- Tests (hermetic, tempdir-backed):
  - `write_blob_returns_correct_digest`
  - `write_blob_is_idempotent` (write same content twice, single file, no error)
  - `write_blob_streaming_verifies_expected`
  - `write_blob_streaming_rejects_digest_mismatch`
  - `put_tag_round_trips`
  - `list_images_returns_persisted_entries`
  - `add_ref_then_referenced_layers_includes_them`
  - `drop_ref_removes_dir`
  - `gc_drops_unreferenced_blobs_keeps_referenced`
- Validate: `cargo test -p wisp-image`.
- Commit: `feat(wisp-image): content store with refcount GC`

### Dispatch B: registry client

Network-touching code. Tests use a wiremock-ish in-process HTTP server (`wiremock` crate is already in the workspace via Phase 12).

#### Step B1: auth challenge + bearer token

- `crates/wisp-image/src/registry/auth.rs`: parse `WWW-Authenticate: Bearer realm=...,service=...,scope=...` headers; fetch the token from the realm. Anonymous (no client credentials in 0.2).
- Public function: `pub fn obtain_token(http: &reqwest::blocking::Client, challenge: &str) -> Result<String>`.
- Tests with wiremock:
  - `parses_realm_service_scope_from_header`
  - `obtain_token_calls_realm_with_scope`
  - `obtain_token_handles_400_with_clear_error`

#### Step B2: manifest + index deserialisation

- `crates/wisp-image/src/registry/manifest.rs`: thin wrappers around `oci_spec::image::ImageManifest` and `oci_spec::image::ImageIndex`. Add `pub enum Manifest { Image(ImageManifest), Index(ImageIndex) }` and a parser that picks the right shape from the response body's `mediaType` field.
- `pub fn select_arch_entry(index: &ImageIndex, target_arch: &str, target_os: &str) -> Option<Descriptor>`. Defaults: target_arch from `std::env::consts::ARCH` mapped to OCI names ("aarch64" -> "arm64", "x86_64" -> "amd64"); target_os "linux".
- Tests with committed JSON fixtures (`crates/wisp-image/tests/fixtures/`):
  - `parses_oci_image_manifest`
  - `parses_oci_image_index`
  - `parses_docker_v2_manifest` (different mediaType)
  - `select_arch_entry_picks_matching_arch`
  - `select_arch_entry_returns_none_when_no_match`

#### Step B3: blob fetch + digest verify

- `crates/wisp-image/src/registry/blob.rs`: `pub fn fetch_blob(http: &reqwest::blocking::Client, url: &str, expected_digest: &str, dest: impl Write) -> Result<u64>`. Streams the response, sha256-hashes alongside, errors on digest mismatch. Returns bytes written.
- Tests with wiremock:
  - `fetch_blob_streams_to_writer`
  - `fetch_blob_verifies_digest`
  - `fetch_blob_rejects_digest_mismatch`
  - `fetch_blob_handles_404_with_clear_error`

#### Step B4: client orchestration

- `crates/wisp-image/src/registry/mod.rs`: `pub struct Client { store, http, base_url }`. `Client::pull(&self, &ImageRef) -> Result<PulledImage>`:
  1. Resolve `<registry>/v2/<repo>/manifests/<tag-or-digest>` (with auth dance if 401).
  2. If response is an Index, select_arch_entry, recurse with the matching digest.
  3. Persist manifest blob + write tag pointer.
  4. Fetch + persist config blob.
  5. Fetch + persist each layer blob.
  6. Return `PulledImage { ref, manifest_digest, config: ImageConfiguration, layers: Vec<LayerRef> }`.
- `Client::lookup`, `list`, `gc` thin wrappers over the store.
- Tests with wiremock simulating Docker Hub:
  - `pull_against_wiremocked_registry`
  - `pull_resolves_index_to_arch_specific_manifest`
  - `pull_skips_already_cached_layers` (set up the store with a layer pre-cached, assert no second download)
  - `pull_with_auth_challenge_negotiates_token`

Dispatch B commit messages:
- `feat(wisp-image): registry auth challenge + bearer token`
- `feat(wisp-image): manifest + index parsing with arch selection`
- `feat(wisp-image): blob fetch with digest verification`
- `feat(wisp-image): registry client orchestration (pull / lookup / list)`

### Dispatch C: layer extraction with whiteouts

#### Step C1: tar / gzip / zstd decode

- `crates/wisp-image/src/store/layer.rs`: `pub fn extract_layer(blob: impl Read, media_type: &str, target: &Path) -> Result<()>`.
- Wraps the reader with the right decompressor:
  - `application/vnd.docker.image.rootfs.diff.tar.gzip` -> `flate2::read::GzDecoder`
  - `application/vnd.oci.image.layer.v1.tar+gzip` -> same
  - `application/vnd.oci.image.layer.v1.tar+zstd` -> `zstd::stream::read::Decoder`
  - `application/vnd.oci.image.layer.v1.tar` -> raw
  - else -> error with the unsupported type embedded
- Walks `tar::Archive::entries`, applies each:
  - Regular files / dirs / symlinks: extract preserving permissions + mtime + xattrs
  - Symlink + hardlink targets: REJECT if they resolve outside `target` (`..` traversal). Standard tar crate has `set_overwrite(true)` + `set_preserve_permissions(true)` + `set_preserve_mtime(true)` + `set_unpack_xattrs(true)` configuration. Must explicitly canonicalize before writing.
- Whiteouts:
  - Entry named `<path>/.wh.<name>`: delete `<target>/<path>/<name>` from previously extracted layers.
  - Entry named `<path>/.wh..wh..opq`: clear all siblings of `<target>/<path>/` first.
  - Whiteout entries themselves are NOT written to disk.
- Tests with hand-built tar fixtures (build via `tar` crate at test setup):
  - `extracts_regular_file_with_mode`
  - `extracts_symlink_inside_target`
  - `rejects_symlink_pointing_outside_target`
  - `rejects_hardlink_pointing_outside_target`
  - `whiteout_deletes_file_from_lower_layer`
  - `opaque_whiteout_clears_siblings_in_directory`
  - `gzip_decodes_correctly`
  - `zstd_decodes_correctly`
  - `unsupported_media_type_errors_with_clear_message`

#### Step C2: multi-layer assembly with atomic finalize

- Add `pub fn assemble_rootfs(store: &ContentStore, layers: &[LayerRef], dest: &Path) -> Result<()>` to `store/layer.rs` (or `bundle.rs`; pick the cleaner home; the spec lists this in `bundle.rs`).
- Sequence:
  1. mkdir `<dest>.partial`
  2. For each layer in order (oldest first), open the blob from the store, call `extract_layer` into `<dest>.partial`.
  3. Atomic rename `<dest>.partial` -> `<dest>`.
- Failure cleanup: rm `<dest>.partial`. Store layer blobs untouched.
- Tests:
  - `assemble_two_layers_overlays_correctly`
  - `assemble_handles_failure_cleanup` (force a layer-extract error via a corrupt blob, assert `<dest>.partial` is removed and `<dest>` doesn't exist)

Dispatch C commits:
- `feat(wisp-image): layer extraction with whiteout handling`
- `feat(wisp-image): multi-layer rootfs assembly with atomic finalize`

### Dispatch D: BundleBuilder + wisp-cli integration

#### Step D1: BundleBuilder

- `crates/wisp-image/src/bundle.rs`:
  ```rust
  pub struct BundleBuilder<'a> { image: &'a PulledImage, store: &'a ContentStore, bundle_dir: PathBuf }

  impl<'a> BundleBuilder<'a> {
      pub fn new(image, store, bundle_dir) -> Self;
      pub fn assemble_rootfs(&self) -> Result<()>;            // calls layer::assemble_rootfs
      pub fn synthesise_config(&self, overrides: ConfigOverrides) -> Result<Spec>;
      pub fn cleanup(&self) -> Result<()>;
  }

  pub struct ConfigOverrides {
      pub args: Option<Vec<String>>,
      pub entrypoint: Option<Vec<String>>,
      pub env: Vec<String>,
      pub cwd: Option<String>,
      pub hostname: Option<String>,
      pub mounts: Vec<oci_spec::runtime::Mount>,
      pub linux_resources: Option<oci_spec::runtime::LinuxResources>,
  }
  ```
- `synthesise_config` baseline: copy the busybox demo's `config.json` shape (5 namespaces, standard mounts, default capabilities) and overlay:
  - `process.args`: `args` override else `image.config.Cmd` else `image.config.Entrypoint`
  - Actually compose entrypoint + cmd properly: if `entrypoint` is set, use it; append `args` (or `image.config.Cmd` if none). If `entrypoint` unset and `image.config.Entrypoint` is set, use it; append `args` or `image.config.Cmd`. Otherwise `args` (if set) or `image.config.Cmd`.
  - `process.env`: `image.config.Env` ++ `overrides.env`
  - `process.cwd`: override else `image.config.WorkingDir` else "/"
  - `hostname`: override else `bundle-id` truncated to 12 chars
  - `mounts`: baseline standard mounts ++ `overrides.mounts`
  - `linux.resources`: override directly
- Tests (Mac OK, no syscalls):
  - `synthesise_config_uses_image_entrypoint_when_no_override`
  - `synthesise_config_args_override_takes_precedence`
  - `synthesise_config_appends_override_env_to_image_env`
  - `synthesise_config_resolves_cwd_default_to_slash`
  - `synthesise_config_default_hostname_truncates_bundle_id`

#### Step D2: wisp-cli image subcommand + --image flag

- `crates/wisp-cli/Cargo.toml`: add `wisp-image` path dep.
- `crates/wisp-cli/src/main.rs`: extend the clap subcommand enum:
  - `Image(ImageCmd)` with subcommands `Pull { ref_str }`, `List`, `Rm { ref_str }`, `Gc`.
  - `Run` gets `--image <ref>` (mutually exclusive with the bundle positional). When set: invoke `Client::pull` -> mkdir `<state-dir>/bundles/<id>/` -> `BundleBuilder::assemble_rootfs` -> `BundleBuilder::synthesise_config` -> write `config.json` -> existing `Runtime::create + start`.
- `wisp run --image alpine:3.19 demo /bin/echo hi` should:
  1. Pull alpine (or hit cache)
  2. Allocate bundle dir
  3. Assemble rootfs from cached layers
  4. Synthesise config with args=["/bin/echo", "hi"]
  5. Runtime::create + start
  6. On Runtime::delete, also drop the bundle dir + content-store ref
- Tests: clap shape via `command().debug_assert()`.

Dispatch D commits:
- `feat(wisp-image): BundleBuilder for image -> bundle synthesis`
- `feat(wisp-cli): image subcommand + --image flag on run`

### Dispatch E: integration tests + demo + release notes

#### Step E1: hermetic tests

- `crates/wisp-image/tests/whiteout_handling.rs`: build two synthetic layer tars in a tempdir, run `assemble_rootfs`, assert whiteout semantics.
- `crates/wisp-image/tests/gc_drops_unreferenced.rs`: pre-populate the store with two manifests sharing two of three layers, drop one image, run gc, assert only the unique layer was removed.
- These are in tests/ not src/ so they're integration targets running against the public API.

#### Step E2: network-required tests (ignored by default)

- `crates/wisp-image/tests/pull_real_image.rs`, `#[ignore]`: pull `docker.io/library/alpine:3.19`, assert layer count + total size + manifest digest match a recorded snapshot. Skipped if `WISP_OFFLINE=1`.
- `crates/wisp-image/tests/roundtrip_run.rs`, `#[ignore]`: pull alpine, `wisp run --image alpine:3.19 demo /bin/sh -c 'echo hi'`, assert stdout contains "hi", exit 0, bundle cleaned up post-delete. Requires root; skip if euid != 0.
- Run via `orb -m wisp -u root bash -c 'PATH=/home/dirdmaster/.cargo/bin:$PATH; cargo test -p wisp-image -- --ignored'`.

#### Step E3: examples + README + release notes

- `crates/wisp-image/examples/pull-alpine.rs`: `Client::new + pull("alpine:3.19") + print PulledImage`.
- `crates/wisp-image/README.md`: what + run + dev loop, mirroring crates/wisp/README.md.
- `docs/RELEASE_NOTES_WISP_PHASE_0_2.md`: scope, done bar, notable design choices (blocking client, refcount GC, hardlink security), known limitations (no auth, no auto-GC).

Dispatch E commits:
- `test(wisp-image): hermetic whiteout + gc tests`
- `test(wisp-image): real-pull and roundtrip integration (ignored by default)`
- `docs(wisp-image): example, README, and phase 0.2 release notes`

## Validation per dispatch

- `cargo build -p wisp -p wisp-cli -p wisp-image` (Mac + VM)
- `cargo test -p wisp-image` (Mac, hermetic tests pass)
- `cargo test -p wisp-image -- --ignored` (VM as root, network-required tests pass)
- `cargo clippy -p wisp-image -p wisp-cli --all-targets -- -D warnings` (both platforms)
- `cargo fmt --check -p wisp-image -p wisp-cli`
- After dispatch E: run the full demo `cargo run -p wisp-cli -- run --image docker.io/library/alpine:3.19 hello /bin/echo hi` from the VM as root; capture output; the `hi` print is the final done-bar.

## Risks

- **Rate limits.** Anonymous Docker Hub allows 100 pulls per 6h per IP. CI tests must `WISP_OFFLINE=1` by default; only manual ad-hoc runs hit the network.
- **Manifest format diversity.** Some old images publish only Docker v1 schema 1; refuse cleanly with "use a newer image" rather than guessing.
- **Path traversal in tar entries.** Critical security boundary. Test the rejection path explicitly.
- **flock vs concurrent pulls.** Two `wisp run --image` invocations of the same uncached image: the second blocks on flock during pull's index update. Tests should cover this.
- **Disk usage drift.** No auto-GC. Document in README that operators schedule `wisp image gc` (or wait for 0.5).
- **reqwest blocking flavor.** Locks in the blocking variant. If a future fleet feature wants the async client too, it's a separate crate.

## Dispatch sequence

| Dispatch | Steps | Estimated session time | Notes |
|---|---|---|---|
| A | A1 / A2 / A3 | medium | Pure Rust, hermetic. Lowest risk. |
| B | B1 / B2 / B3 / B4 | medium | wiremock-based; some real-world manifest format wrangling. |
| C | C1 / C2 | medium | Whiteout semantics + tar security. Sonnet review at the end. |
| D | D1 / D2 | medium | Touches wisp-cli; verify bundle synthesis matches what wisp-runtime expects. |
| E | E1 / E2 / E3 | small | Tests, demo, docs. Verify the demo runs end-to-end. |

After dispatch E, the done-bar is `wisp run --image docker.io/library/alpine:3.19 demo /bin/echo hi` printing `hi` from the OrbStack VM as root.

## Open questions during implementation

- Whether to put `bundle.rs` and `assemble_rootfs` in the same crate (`wisp-image`) or expose `assemble_rootfs` from `wisp-image::store::layer` and have `bundle.rs` be thinner. Decision deferred to dispatch C / D; pick what reads cleaner.
- How aggressive the layer-extract should be about preserving xattrs for setcap-bestowed binaries. Default to preserving everything tar exposes.
- `oci-spec::image::ImageConfiguration` field names vs Docker's. The crate may not expose `Cmd` and `Entrypoint` with those exact names; pick the Rust API and document the mapping.
- Whether to spawn layer fetches in parallel (faster) vs sequentially (simpler, locks the single-thread invariant). Stick with sequential for 0.2; revisit in 0.3 if pull latency hurts the demo.
