// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Read CARGO_MANIFEST_DIR at RUNTIME, not with env!(). env!() bakes the
    // path in when the build script itself is compiled, so a build-script
    // binary cached from another checkout (a git worktree, a moved clone)
    // keeps pointing at wherever it was first built and every later build
    // fails with "Could not make proto path relative" against a directory that
    // no longer exists. Cargo always sets this variable when it runs us.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest.join("../../../proto");
    let proto_files = [
        proto_root.join("uc/v1/errors.proto"),
        proto_root.join("uc/v1/agent.proto"),
        proto_root.join("uc/v1/tools.proto"),
    ];

    for f in &proto_files {
        println!("cargo:rerun-if-changed={}", f.display());
    }

    let out_dir = manifest.join("src/generated");
    std::fs::create_dir_all(&out_dir)?;

    let descriptor_path = out_dir.join("descriptor.bin");

    prost_build::Config::new()
        .out_dir(&out_dir)
        .file_descriptor_set_path(&descriptor_path)
        .protoc_arg("--experimental_allow_proto3_optional")
        .extern_path(".google.protobuf.Duration", "::pbjson_types::Duration")
        .extern_path(".google.protobuf.Timestamp", "::pbjson_types::Timestamp")
        .compile_protos(&proto_files, &[proto_root])?;

    pbjson_build::Builder::new()
        .register_descriptors(&std::fs::read(&descriptor_path)?)?
        .out_dir(&out_dir)
        .build(&[".uc.v1"])?;

    Ok(())
}
