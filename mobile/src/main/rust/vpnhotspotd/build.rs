use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let proto_dir = "../../proto";
    let proto = format!("{proto_dir}/daemon.proto");
    println!("cargo:rerun-if-changed={proto}");

    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .compile_protos(&[proto], &[proto_dir])?;
    Ok(())
}
