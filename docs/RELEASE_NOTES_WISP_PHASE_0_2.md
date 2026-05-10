# Wisp Phase 0.2: image pulling (`wisp-image`)

> Phase 0.2 of wisp. Branch: `wisp/phase-0-1`. Builds on 0.1's runtime; not in `next` yet, no operator-facing change for v0.3.x users.

## What this is

`crates/wisp-image/` is the OCI distribution + content store + bundle synthesis layer that sits underneath `crates/wisp/` (the runtime). Operator points at a registry reference like `alpine:3.19`; the crate fetches the manifest + config + layers, persists everything in a content-addressable cache, and synthesises a runtime bundle (`config.json` + `rootfs/`) that `wisp::Runtime` can `create + start`.

`wisp-cli` grows two surfaces: `wisp run --image <ref>` (pull then run, with cleanup on foreground exit) and a `wisp image pull | list | rm | gc` subcommand group for managing the cache directly.

This is the second of four phases on the way to v0.4. 0.1 was the runc-equivalent (run a hand-prepared bundle); 0.2 is image pulling; 0.3 is networking; 0.4 wires it into the Isengard agent.

## What's NOT in 0.2

- **No auth.** Anonymous public registries only. No `wisp login`, no docker-config credential lookup, no GHCR PATs, no ECR. Deferred to 0.5 alongside private-registry support.
- **No image building.** Use `buildkit` / `nerdctl build` / Docker and push the resulting image to a registry. We never plan to own the build path.
- **No overlay storage.** Layers are flattened into a single rootfs at extraction time. `overlayfs` mounts and copy-on-write are deferred; the demo bar doesn't need them and they're a separate body of work.
- **No cross-architecture pulls.** Multi-arch manifest indexes ARE handled (we pick the host arch), but the runtime can't run a foreign-arch image. Pulling `arm64` from an `amd64` host succeeds but the bundle won't start.
- **No automatic garbage collection.** Operators must run `wisp image gc` (or wait for 0.5). The CLI doesn't garbage-collect on its own to keep `wisp run --image` predictable.
- **No content trust / signature verification.** Sigstore / cosign integration is a 1.0 line item.

## Done bar

`wisp run --image docker.io/library/alpine:3.19 --id hello /bin/echo hi` from inside the OrbStack `wisp` VM as root prints exactly `hi` and exits 0. Verified end-to-end on Ubuntu 24.04, kernel 6.19, cgroup v2, arm64 (the same VM that proves out 0.1).

Verbatim demo output:

```text
$ rm -rf /var/lib/wisp-demo
$ WISP_STATE_DIR=/var/lib/wisp-demo target/debug/wisp \
    run --image docker.io/library/alpine:3.19 --id hello /bin/echo hi
hi
```

The first invocation pulls alpine from Docker Hub (~3MB on arm64); re-runs hit the local cache and start in well under a second.

Test counts on `cargo test -p wisp-image --tests`:

- Mac (hermetic): 81 unit + 1 hermetic whiteout + 1 hermetic gc = 83 green; 2 ignored (pull + roundtrip).
- VM as root with `--ignored`: pull + roundtrip both pass. The roundtrip pulls alpine, drives `wisp::Runtime` through create + start + stop + delete, and asserts captured stdout matches the echo content.

## Public API

```rust
use wisp_image::{BundleBuilder, Client, ConfigOverrides, ImageRef};

let client = Client::new(&store_dir)?;
let r: ImageRef = "alpine:3.19".parse()?;
let pulled = client.pull(&r)?;

let bundle = BundleBuilder::new(&pulled, client.store(), &bundle_dir);
bundle.assemble_rootfs()?;                  // <bundle>/rootfs/ from layers
bundle.write_config(ConfigOverrides {       // <bundle>/config.json
    args: Some(vec!["/bin/echo".into(), "hi".into()]),
    ..Default::default()
})?;
client.store().add_ref("hello", &pulled.layers
    .iter().map(|l| l.digest.clone()).collect::<Vec<_>>())?;

// Now `wisp::Runtime::create("hello", &bundle_dir)` runs the bundle.
```

`ConfigOverrides` is additive in spirit: empty fields keep the image's own entrypoint / cmd / env / cwd / hostname; mounts and env are appended; args / entrypoint / cwd / hostname / linux_resources replace.

## Disk layout

```text
<state-dir>/
  images/
    blobs/sha256/<hex>          # content-addressed blob (manifest / config / layer)
    index/<registry>/<repo-segs>/tag/<tag>   # tag pointer -> manifest digest
    refs/<bundle-id>/layers     # newline-separated layer digests pinned by bundle
    .lock                       # advisory file lock for cross-process pulls
  bundles/<id>/
    config.json                 # synthesised runtime spec
    rootfs/                     # extracted layered filesystem
  containers/<id>/              # wisp::Runtime state-dir entry (Phase 0.1)
```

The pull path is the only writer of `images/blobs`. Atomic rename via `tempfile::NamedTempFile::persist` + an exclusive flock on `.lock` keep concurrent pulls coherent. Reads are lock-free.

## Notable design choices

- **Blocking reqwest.** `wisp::Runtime::start` calls `clone3` and the calling thread must be single-threaded (glibc malloc deadlock at multi-threaded clone). `wisp run --image` is a single binary that does `pull` then `start` back-to-back, so we keep the pull path off `tokio` entirely. `reqwest::blocking` over `rustls` is enough; the OCI distribution API is request-per-blob with no streaming idioms that benefit from async, and alpine-sized pulls finish in single-digit seconds anyway.
- **Refcount GC.** The store has no separate index-of-blobs file. `gc` walks `index/` and `refs/` to compute the keep-set, then deletes everything else under `blobs/`. This rebuilds from disk every call, which is slow at GB scale but bullet-proof against partial writes (no index to corrupt).
- **Lexical path-traversal protection.** Tar entry paths and link targets are walked component-by-component (`Component::ParentDir => return None`) before being joined onto the canonical extraction root. We never `canonicalize` an entry path because it doesn't exist on disk yet at validation time.
- **Layered tar with OCI whiteouts.** `<dir>/.wh.<name>` deletes `<dir>/<name>` from the underlying filesystem; `<dir>/.wh..wh..opq` clears every direct child of `<dir>`. The whiteout entries themselves are NOT written to disk. Hermetic test in `tests/whiteout_handling.rs` exercises this end-to-end through the public `Client + BundleBuilder` API.
- **Real-image symlink/hardlink handling.** Alpine emits 300+ absolute symlinks like `bin/sh -> /bin/busybox` (kernel resolves them against the namespace root after `pivot_root`, so they're correct in the running container) and a handful of absolute hardlink targets. We accept absolute symlink targets verbatim; absolute hardlink targets get a leading-slash strip and are joined onto the rootfs root. Path traversal via `..` is still rejected. Tightening this to use `openat`-relative ops (so a malicious image can't write through a directory symlink during extraction) is tracked for 0.5.

## Known limitations

- **Anonymous Docker Hub rate limit.** 100 pulls per 6h per IP. The `pulls_alpine_3_19` integration test is `#[ignore]`d by default for this reason; CI defaults to `WISP_OFFLINE=1`.
- **No auth.** As above. Private registries don't work; you'll get a 401 with no recovery path.
- **Manual GC.** Operators run `wisp image gc` themselves. Without a periodic timer, layer blobs accumulate.
- **No buildkit.** Operators build images elsewhere and `pull` them.
- **Blocking client.** The single-threaded clone3 invariant keeps us off async. If a future fleet feature wants the async client too, it's a separate crate.
- **Old image formats.** Docker Hub still ships some images with schema 1 manifests; we refuse them with a clean error rather than guessing. Use a recent image (any image pushed by the docker.io official builders in the last several years works).
- **Cross-arch pulls succeed but won't run.** Multi-arch indexes resolve to the host arch automatically; explicit `--platform` plumbing is deferred to 0.3.

## Spec + plan

- Spec: [`docs/superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md`](superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md)
- Plan: [`docs/superpowers/plans/2026-05-09-wisp-phase-0-2-image-pulling.md`](superpowers/plans/2026-05-09-wisp-phase-0-2-image-pulling.md)
- 0.1 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_1.md`](RELEASE_NOTES_WISP_PHASE_0_1.md)
