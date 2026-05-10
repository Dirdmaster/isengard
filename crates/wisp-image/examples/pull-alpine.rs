//! Minimal pull example: drive `wisp_image::Client::pull` against
//! Docker Hub and print the resolved manifest summary.
//!
//! Usage:
//!
//! ```bash
//! WISP_STATE_DIR=/var/lib/wisp-demo cargo run --example pull-alpine
//! # or with the default cache location:
//! cargo run --example pull-alpine
//! ```
//!
//! Defaults to `/tmp/wisp-image-demo` for the cache when
//! `WISP_STATE_DIR` is unset, so the example can run against a
//! tempfs-style location without touching `/var/lib`.

use wisp_image::{Client, ImageRef};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let store_dir = std::env::var("WISP_STATE_DIR")
        .map(|d| std::path::PathBuf::from(d).join("images"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/wisp-image-demo"));
    let client = Client::new(&store_dir)?;
    let r: ImageRef = "docker.io/library/alpine:3.19".parse()?;
    let pulled = client.pull(&r)?;
    println!(
        "pulled {} (manifest {}, {} layer(s))",
        pulled.r,
        pulled.manifest_digest,
        pulled.layers.len()
    );
    for layer in &pulled.layers {
        println!("  layer {} {} bytes", layer.digest, layer.size);
    }
    Ok(())
}
