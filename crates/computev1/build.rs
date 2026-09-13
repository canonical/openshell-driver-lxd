// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../proto");
    let compute_proto = proto_dir.join("compute_driver.proto");
    let sandbox_proto = proto_dir.join("sandbox.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[compute_proto.clone(), sandbox_proto.clone()],
            &[proto_dir],
        )?;
    println!("cargo:rerun-if-changed={}", compute_proto.display());
    println!("cargo:rerun-if-changed={}", sandbox_proto.display());
    Ok(())
}
