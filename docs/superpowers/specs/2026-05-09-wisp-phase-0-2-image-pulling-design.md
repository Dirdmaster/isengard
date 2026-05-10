# Wisp Phase 0.2: `wisp-image` MVP (design)

Status: proposed
Phase: v0.4 foundation, wisp 0.2
Author: 2026-05-09
Depends on: wisp 0.1 (the runc-equivalent runtime, branch `wisp/phase-0-1`)

## Problem

Phase 0.1 ships a runtime that runs OCI bundles, but bundles are still hand-prepared: the operator extracts a tarball, hand-writes `config.json`, and lives with whatever's in `rootfs/`. That doesn't survive contact with real workloads. Pulling images from registries is the next missing layer.

Without `wisp-image` we can't:
- Run containers from public registries (`nginx:alpine`, `postgres:17`).
- Cache layers across containers (every fresh bundle re-extracts the same tarballs).
- Reuse the OCI image's `config.Config` (entrypoint, env, cwd, exposed ports, labels) so spec generation isn't a 50-line dance per container.
- Garbage-collect unused layers when state-dir bloats.

Phase 0.2 adds a Rust crate that does exactly that and nothing more: pull, store, and unpack images so `wisp-runtime` can run them.

## Goal

Land `crates/wisp-image/` (library) plus a `wisp-cli image` subcommand. After this phase:

```
wisp image pull docker.io/library/nginx:alpine
wisp image list
wisp run --image nginx:alpine my-nginx
```

Where `wisp run --image` synthesises a bundle on demand by:
1. Looking up the image in the local store.
2. Layering the rootfs into a fresh directory under `<state-dir>/bundles/<id>/rootfs`.
3. Generating a `config.json` from the image's `config.Config` plus operator overrides (entrypoint, env, mounts, etc.).
4. Calling the existing `Runtime::create + start`.

Phase 0.2 done bar: `wisp run --image docker.io/library/alpine:3.19 hello /bin/echo hi` pulls the image (or hits cache), assembles a bundle, runs it, prints `hi`, exits clean. State-dir layout post-run: image layers cached, bundle dir cleaned up on `wisp delete`.

## Non-goals (Phase 0.2)

- Private registries (auth). Phase 0.5+ adds Docker config.json + token negotiation. 0.2 supports public registries only (the anonymous Docker Hub flow + GHCR public).
- Image building (`docker build` equivalent). Out of scope forever; we orchestrate, we don't build. Operator builds elsewhere and pushes to a registry.
- Cross-arch image pulling. Pull the manifest matching the host arch (arm64 on the OrbStack VM, amd64 on x86 Linux). Multi-arch manifest lists are read; the matching architecture is selected.
- Image signing / cosign / sigstore verification. Phase 0.6+ if it becomes load-bearing.
- Overlay storage driver. 0.2 uses simple per-bundle directory copies (or hardlinks where possible). `wisp-storage` with overlayfs is Phase 0.3.
- ZSTD layer support beyond what `oci-spec` already specifies. Most public images are gzip; if zstd shows up, `flate2` + `zstd` crate handles it inline.
- Image GC is reference-counted (a bundle holds refs to its layers, layers stay alive while any bundle references them). Time-based pruning + GC schedulers come later.

## Design

### Crate layout

```
crates/
  wisp-image/                 # the image library
    Cargo.toml                # NO isengard-* deps; depends on wisp for shared types
    src/
      lib.rs                  # public API
      error.rs                # WispImageError (re-exports as wisp::WispError variant?)
      reference.rs            # parse "registry/name:tag" -> components
      registry/
        mod.rs                # public client surface
        auth.rs               # token negotiation against /v2/ challenges
        manifest.rs           # manifest + manifest-list deserialisation
        blob.rs               # blob fetch with content-length + digest verify
      store/
        mod.rs                # ContentStore: blob-by-digest CAS layout
        layer.rs              # layer extraction (tar untar)
        meta.rs               # image metadata index (image -> manifest digest -> layers)
      bundle.rs               # generate config.json from image config + assemble rootfs
    examples/
      pull-alpine.rs          # `cargo run --example pull-alpine` smoke
    tests/
      pull_real_image.rs      # network-required, ignored by default
```

Wires into `wisp-cli` as a new `wisp image <subcommand>` group plus the `--image` flag on `wisp run`.

### Public API

```rust
// crates/wisp-image/src/lib.rs

pub struct ImageRef {
    pub registry: String,      // "docker.io"
    pub repo: String,          // "library/nginx"
    pub tag: Option<String>,   // "alpine"
    pub digest: Option<String>, // "sha256:..."  (mutually exclusive with tag)
}

pub struct Client {
    store: ContentStore,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(store_dir: &Path) -> Result<Self, WispImageError>;

    /// Resolve manifest, fetch missing blobs, persist to store. Idempotent.
    pub fn pull(&self, r: &ImageRef) -> Result<PulledImage, WispImageError>;

    /// Already-cached check; useful for `wisp image list`.
    pub fn lookup(&self, r: &ImageRef) -> Result<Option<PulledImage>, WispImageError>;

    pub fn list(&self) -> Result<Vec<PulledImage>, WispImageError>;

    /// Reference-counted layer GC: drops unreferenced layer blobs. The
    /// store keeps a manifest -> layer-digests index plus a refcount per
    /// layer; a "ref" is held by any bundle dir that referenced it.
    pub fn gc(&self) -> Result<GcReport, WispImageError>;
}

pub struct PulledImage {
    pub r: ImageRef,
    pub manifest_digest: String,
    pub config: oci_spec::image::ImageConfiguration,
    pub layers: Vec<LayerRef>,
}

pub struct LayerRef {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}
```

```rust
// bundle.rs (separate because it's how wisp-runtime consumes wisp-image)

pub struct BundleBuilder<'a> {
    image: &'a PulledImage,
    bundle_dir: PathBuf,
}

impl<'a> BundleBuilder<'a> {
    pub fn new(image: &'a PulledImage, bundle_dir: &Path) -> Self;

    /// Layer the image into <bundle_dir>/rootfs. Idempotent: if rootfs
    /// already exists, no-op. Per-layer extraction respects whiteouts
    /// per OCI image spec.
    pub fn assemble_rootfs(&self) -> Result<(), WispImageError>;

    /// Generate config.json from image.config + operator overrides.
    /// Returns the resulting Spec (so caller can serialise).
    pub fn synthesise_config(
        &self,
        overrides: ConfigOverrides,
    ) -> Result<oci_spec::runtime::Spec, WispImageError>;

    /// Delete <bundle_dir>/rootfs. Layers are NOT touched (stay in store).
    pub fn cleanup(&self) -> Result<(), WispImageError>;
}

pub struct ConfigOverrides {
    pub args: Option<Vec<String>>,    // overrides image.config.Cmd
    pub entrypoint: Option<Vec<String>>, // overrides image.config.Entrypoint
    pub env: Vec<String>,              // appended to image.config.Env
    pub cwd: Option<String>,           // overrides image.config.WorkingDir
    pub hostname: Option<String>,
    pub mounts: Vec<oci_spec::runtime::Mount>,  // additional bind mounts
    pub linux_resources: Option<oci_spec::runtime::LinuxResources>,
}
```

### Content store layout

Store at `<state-dir>/images/`:

```
<state-dir>/images/
  blobs/
    sha256/
      <hex>                       # raw blob: manifest, config, or layer
  index/
    <registry>/<repo>/
      tag/
        <tag>                     # 1-line file: "sha256:<manifest-digest>"
      digest/
        sha256/<hex>              # symlink (or 1-line file) for digest pulls
  refs/
    <bundle-id>/
      layers                      # newline-separated layer digests this
                                  # bundle holds refs to (for GC)
```

Atomic writes for any store mutation: write to `<path>.tmp`, fsync, rename. Pulls under flock at the index level so concurrent `wisp image pull` doesn't corrupt the manifest pointer.

### Reference parsing (reference.rs)

```rust
/// Accepts:
///   - "alpine"                           -> docker.io/library/alpine:latest
///   - "nginx:alpine"                     -> docker.io/library/nginx:alpine
///   - "library/redis:7.4"                -> docker.io/library/redis:7.4
///   - "ghcr.io/dirdmaster/foo:bar"       -> ghcr.io/dirdmaster/foo:bar
///   - "docker.io/library/alpine@sha256:.." -> digest pull
pub fn parse(s: &str) -> Result<ImageRef, WispImageError>;
```

Default registry: `docker.io`. Default tag: `latest`. Library namespace inferred for single-component names. Digest pulls bypass the tag.

### Registry client (registry/)

`reqwest::blocking` to keep this on the same single-thread invariant as wisp-runtime (clone3 forbids tokio). Pull sequence:

1. **GET** `<registry>/v2/`. If 401, parse `WWW-Authenticate: Bearer realm=...,service=...,scope=...`.
2. **GET** the bearer token from the realm with the requested scope. No client credentials in 0.2 (anonymous).
3. **GET** `<registry>/v2/<repo>/manifests/<tag>` with `Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json`. Inspect content-type:
   - If manifest list / index: pick the entry matching host arch + os; loop with that digest.
   - If image manifest: parse, extract config digest + layer digests.
4. **GET** the config blob `<registry>/v2/<repo>/blobs/<config-digest>`. Verify content matches the digest. Persist to store.
5. **GET** each layer blob in parallel-ish (sequential for 0.2 to keep simple; switch to a small pool in 0.3 if pull latency hurts). Verify digest. Persist to store.
6. Persist the manifest itself to `blobs/sha256/<manifest-digest>`. Update `index/<registry>/<repo>/tag/<tag>` to point at the manifest digest.
7. Return `PulledImage`.

Errors: `WispImageError::Registry { status, body }` for non-2xx with the registry's error message preserved. `WispImageError::DigestMismatch { expected, got }` for verification failures.

### Layer extraction (store/layer.rs)

Each layer is a tar (or tar+gzip / tar+zstd) per the manifest's `mediaType`. Standard logic:

1. Decompress (`flate2::read::GzDecoder` or `zstd::stream::read::Decoder`).
2. Walk tar entries via `tar::Archive`. Apply each entry to the rootfs:
   - Regular files / dirs / symlinks: extract.
   - Whiteouts (entries named `.wh.<name>`): delete the named entry on the rootfs.
   - Opaque whiteouts (`.wh..wh..opq` in a directory): clear the directory's siblings before applying.
3. Use `tar` with the `unpack` option preserving permissions + mtime + xattrs (extended attrs needed for setcap-bestowed binaries).

Per-layer extraction is into a temp dir, then atomically renamed under the bundle's rootfs:

```
<bundle-dir>/rootfs.partial/
  ... extracted layers ...
mv rootfs.partial rootfs
```

Failure cleanup: rm rootfs.partial, leave the store untouched.

### Bundle assembly (bundle.rs)

`assemble_rootfs`:
1. mkdir `<bundle-dir>/rootfs.partial`
2. For each layer in order (oldest to newest), call layer extraction
3. Apply OCI image-spec post-extraction rules (clear /etc/resolv.conf if not in image, set up /etc/hostname, etc. Defer most of these to 0.3 with `wisp-net`; for 0.2 we just leave the rootfs as the layered tarball implies).
4. Atomic rename to `<bundle-dir>/rootfs`.

`synthesise_config`:
1. Start from a baseline `oci_spec::runtime::Spec` (the same minimal one wisp-runtime accepts: 5 namespaces, standard mounts, default capabilities).
2. Set `process.args` from the override, falling back to `image.config.Entrypoint + image.config.Cmd`.
3. Set `process.env` from `image.config.Env` plus the override's appended env.
4. Set `process.cwd` from override or `image.config.WorkingDir`.
5. Set `hostname` from override or default to `bundle-id` truncated to 12 chars (matches Docker's behavior for unset hostnames).
6. Append mounts from the override.
7. Apply `linux.resources` override directly.
8. Return the Spec; caller serialises to `<bundle-dir>/config.json`.

### `wisp-cli` integration

New subcommands:

```
wisp image pull <ref>                   # pulls + caches
wisp image list                         # all cached images
wisp image rm <ref>                     # remove from index; layers GC'd if unreferenced
wisp image gc                           # explicit gc pass

wisp run --image <ref> <id> [-- args...]   # synthesise bundle + run
```

`wisp run --image <ref> <id>` flow:
1. `Client::pull(ref)` (no-op if cached).
2. Allocate `<state-dir>/bundles/<id>/`.
3. `BundleBuilder::assemble_rootfs`.
4. `BundleBuilder::synthesise_config(overrides_from_args)` -> write `config.json`.
5. Call existing `Runtime::create + start` against this bundle dir.
6. On `Runtime::delete`, also delete the bundle dir; layers stay (refcounted in `refs/<id>/layers`).

Backward compat: `wisp run <bundle>` (positional bundle dir) keeps working unchanged.

## Test strategy

### Unit tests (Mac OK, no network)

- `reference::parse` covers: bare name, name + tag, namespaced, registry-explicit, digest pull, malformed input rejection.
- `manifest::parse_manifest` and `parse_manifest_list` against canonical OCI fixtures (committed under `tests/fixtures/`).
- `store::layout` round-trips: write a fake blob, read by digest, list, remove (refcount).
- `bundle::synthesise_config` against a hand-built `ImageConfiguration` fixture: assert resulting Spec has the right entrypoint, env, cwd.

### Integration tests (Linux VM, network-required)

- `pull_real_image.rs`, `#[ignore]` by default: pull `docker.io/library/alpine:3.19`, assert layer count + total size + manifest digest match a recorded snapshot. Skipped if `WISP_OFFLINE=1` is set.
- `roundtrip_run.rs`, `#[ignore]`: pull alpine, `wisp run --image alpine:3.19 demo /bin/sh -c 'echo hi'`, assert stdout contains `hi`, exit 0, bundle cleaned up post-delete.
- `whiteout_handling.rs`, hermetic: build a synthetic 2-layer tar pair where layer 2 contains a whiteout for a file in layer 1, run extraction, assert the file is absent in the result.
- `gc_drops_unreferenced.rs`, hermetic: write two image manifests sharing two of three layers, remove one image, assert only the unique layer is GC'd.

### Network test isolation

Network tests are ignored by default (`cargo test` skips them). `cargo test -- --ignored` runs them. The CI question gets revisited once we have a stable set; for 0.2, "the operator can run them on the VM with internet" is the bar.

## Risks

- **Registry rate limits.** Anonymous Docker Hub pulls are rate-limited (100/6h per IP). Tests that pull will trip this if run in a tight loop. Mitigation: cache aggressively in CI and use `WISP_OFFLINE=1` for default test runs.
- **Manifest format diversity.** Docker v2 manifest, OCI v1 manifest, manifest list, image index. The Accept header negotiation handles most paths but real images surface edge cases (some old images publish only Docker v1 schema 1, which we will refuse with a clear error pointing at "use a newer image" or "rebuild your image").
- **Layer media types.** Most public layers are `application/vnd.docker.image.rootfs.diff.tar.gzip` or the OCI equivalent. Zstd layers are growing but rare; we support the common path and error loudly on unknown types.
- **Symlink + hardlink extraction.** Tar archives can pack hardlinks pointing at out-of-rootfs paths (security concern). Our extractor must enforce that all targets resolve under the rootfs (reject `..` traversal). Standard `tar` crate logic handles this with the right options.
- **Refcount races.** Two `wisp run` invocations pulling the same image at once: file-locking around the index. We hold an flock during pull's index update; concurrent reads are fine.
- **Disk usage.** Cached layers grow without bound until `wisp image gc`. 0.2 doesn't auto-GC. Operator runs `gc` manually or via cron. 0.5 schedules it.

## Stretch goals (if 0.2 lands fast)

- **Authenticated pulls** via `~/.docker/config.json` with the same creds dockerd uses. Trivial once the bearer-token plumbing is in place; just feed credentials into the auth step.
- **Private registry** (GHCR with a token, internal mirrors).
- **Hardlink layer reuse** when the same layer digest is in multiple bundles: layer extraction creates hardlinks to a canonical store path instead of copying. Saves disk + IO; needs the rootfs path on the same filesystem as the store.
- **`isd` integration** to surface image cache size + last-pull-time in the dashboard.

## Out of scope, explicitly

- Building images. Never wisp's job.
- Pushing images. Never wisp's job.
- Image scanning / vulnerability detection. Out of scope (orchestrators above us, like Renovate or Trivy GitHub Action).
- Cross-architecture pulls + emulation (`qemu-user-static`). Operator runs the right arch on the right host.

## Open questions

- **Async vs blocking.** Phase 0.1 used blocking + single-threaded invariant for clone3. The image client could be tokio-based for parallel layer fetches (faster pulls) but then it lives in a separate process from the runtime. Decision: blocking for 0.2. If pull latency is real we revisit in 0.3 by either: (a) spawning the pull in a separate process, or (b) doing the pull before any clone3 happens (which is the natural lifecycle anyway).
- **Where image cache lives.** Phase 0.1 puts state-dir at `/var/lib/wisp/`. Image cache could share that or split to `/var/lib/wisp-image/`. Sharing keeps the operator surface small; splitting makes "delete all images" a single rm. Default: share at `/var/lib/wisp/images/`.
- **ImageRef serialization.** Should `wisp ps` and `wisp state` show the original ref or the resolved digest? Both. Show ref + ` (sha256:abcd...)` truncated. Discussed during implementation.
- **Crate dependencies.** `reqwest` (blocking, rustls-tls), `flate2`, `zstd`, `tar`, `sha2`, `hex`, `oci-spec` (already in workspace), `tracing`. Locks `reqwest` to its blocking flavor; if a future fleet feature wants the async client too, it lives in a different crate.
