# wisp-image

## What

`wisp-image` is the OCI distribution + content store + bundle synthesis
crate that sits underneath `wisp` (the runtime). It pulls images from
public registries (anonymous; no auth in 0.2), maintains a
content-addressable layer cache with refcount GC, extracts layered
filesystems with OCI whiteout semantics, and synthesises a runtime
bundle (`config.json` + `rootfs/`) that `wisp::Runtime` can
`create` + `start`.

## Why

Phase 0.1 of wisp ran hand-prepared OCI bundles only. To get to a
production runtime, the operator (or a higher-level tool) needs a way
to point at a registry reference like `alpine:3.19`, fetch the
manifest + layers, and end up with a bundle dir on disk. That's the
job of this crate. It owns the on-disk image cache so a second
`wisp run --image alpine:3.19` is a hit, not a re-pull.

`wisp-image` has zero `isengard-*` dependencies and is consumed only by
`wisp-cli` and through `cargo run --example pull-alpine`.

## Status

Phase 0.2, alpha. Demo runs end-to-end on the OrbStack `wisp` VM as
root. Anonymous public registries only (no `wisp login`, no private
registries; deferred to 0.5). cargo build + clippy clean on Mac and
arm64 Linux.

## Run the demo

The demo path: an OrbStack VM named `wisp` running Ubuntu 24.04 (or
similar cgroup v2 distro), the workspace bind-mounted from the Mac.
Inside the VM as root:

```sh
orb -m wisp -u root bash
PATH=/home/dirdmaster/.cargo/bin:$PATH
cd /Users/dirdmaster/Projects/isengard/.worktrees/next
cargo run -p wisp-cli -- run --image docker.io/library/alpine:3.19 --id hello /bin/echo hi
# expected output: hi
```

The first invocation pulls alpine from Docker Hub (~3MB on arm64),
synthesises a bundle, and runs `/bin/echo hi` inside it. Re-runs hit
the local cache and start almost immediately.

You can also exercise the pull path on its own from outside the VM
(no root required, no runtime invocation):

```sh
WISP_STATE_DIR=/tmp/wisp-image-demo cargo run -p wisp-image --example pull-alpine
# pulled docker.io/library/alpine:3.19 (manifest sha256:..., 1 layer(s))
#   layer sha256:... 3359301 bytes
```

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

| Method | Description |
|--------|-------------|
| `Client::new(store_dir)` | Open or create the on-disk image cache. |
| `Client::pull(r)` | Pull `r` from its registry, persist manifest + config + layers, return a summary. |
| `Client::lookup(r)` | Return the cached `PulledImage` for `r`, or `None`. |
| `Client::list()` | Walk the index, return every `(image, manifest_digest)` pair. |
| `Client::gc()` | Drop blobs neither referenced by a bundle nor pointed at by the index. |
| `BundleBuilder::assemble_rootfs()` | Layer extraction into `<bundle>/rootfs/`. Whiteout-aware. |
| `BundleBuilder::write_config(overrides)` | Synthesise `<bundle>/config.json` from the image config + overrides. |
| `BundleBuilder::cleanup()` | Remove `<bundle>/rootfs/`. Layer blobs untouched. |
| `ContentStore::add_ref(id, digests)` | Pin `digests` against bundle `id` so `gc` won't drop them. |
| `ContentStore::drop_ref(id)` | Release `id`'s pin. |

## Cache layout

```text
<store_dir>/
  blobs/sha256/<hex>          # content-addressed blob (manifest, config, or layer)
  index/<registry>/<repo-segs>/tag/<tag>   # tag pointer -> manifest digest
  refs/<bundle-id>/layers     # newline-separated list of layer digests
  .lock                       # advisory file lock for cross-process pulls
```

`pull` writes `blobs/sha256/<hex>` for the manifest, the image config,
and every layer (atomic via `tempfile::NamedTempFile::persist`). Tag
pulls also write the index pointer; digest pulls don't (the digest IS
the canonical key). `assemble_rootfs` reads layer blobs by digest and
extracts them in oldest-to-newest order into `<bundle>/rootfs.partial`,
then atomically renames to `<bundle>/rootfs`.

## Roadmap

- 0.3: networking primitives (`wisp-net`).
- 0.4: agent integration. The Isengard agent stops talking to dockerd
  and drives `wisp::Runtime` + `wisp_image::Client` directly.
- 0.5: registry auth (Docker Hub creds, GHCR PATs, ECR). Auto-GC.
  Image build is intentionally never our job (use `buildkit` /
  `nerdctl build` and push the resulting image to a registry).

For the 0.2 spec + plan see
[`docs/superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md`](../../docs/superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md)
and
[`docs/superpowers/plans/2026-05-09-wisp-phase-0-2-image-pulling.md`](../../docs/superpowers/plans/2026-05-09-wisp-phase-0-2-image-pulling.md).

## License

MIT (matches the workspace).
