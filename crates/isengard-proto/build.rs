//! Compiles `proto/isengard.v1.proto` at build time.
//!
//! Runs `tonic-build` with both client and server codegen enabled, and
//! drops an encoded `FileDescriptorSet` in `OUT_DIR` so the controller
//! can expose the schema over gRPC reflection (the `grpcurl` workflow
//! works without a local copy of the .proto file).
//!
//! Rebuilds trigger on changes to `proto/isengard.v1.proto` and
//! `build.rs` itself; nothing else.

use std::env;
use std::path::PathBuf;

/// Entry point Cargo invokes.
///
/// Reads `OUT_DIR` from the environment, drives `tonic_build`, and
/// writes the descriptor set to `<OUT_DIR>/isengard_descriptor.bin`.
/// The descriptor path matches the constant
/// [`isengard_proto::FILE_DESCRIPTOR_SET`] reads back at runtime.
///
/// # Errors
///
/// Returns `Err` when `OUT_DIR` is missing or `tonic_build` fails to
/// compile the .proto (syntax error, unknown import, etc.). Cargo
/// surfaces the error in build output and aborts the crate compile.
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
