//! Compiles `proto/isengard.v1.proto` to Rust at build time, plus emits a
//! file descriptor set for runtime reflection.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .file_descriptor_set_path(out_dir.join("isengard_descriptor.bin"))
        .compile_protos(&["proto/isengard.v1.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/isengard.v1.proto");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
