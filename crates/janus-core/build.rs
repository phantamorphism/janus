//! Generates the Rust form of the plugin contract's wire types from
//! `proto/janus.proto` (client and server stubs both: the daemon is the
//! client, and Rust plugins serving the out-of-process models are servers).
//! Requires `protoc` on `PATH` or in `PROTOC`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../proto");
    let proto = proto_dir.join("janus.proto");
    println!("cargo:rerun-if-changed={}", proto.display());
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &[proto_dir])?;
    Ok(())
}
