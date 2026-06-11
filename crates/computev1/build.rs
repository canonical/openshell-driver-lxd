// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../proto");
    let proto_file = proto_dir.join("compute_driver.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(std::slice::from_ref(&proto_file), &[proto_dir])?;
    println!("cargo:rerun-if-changed={}", proto_file.display());
    Ok(())
}
