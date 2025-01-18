use::std::error::Error;
use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    tonic_build::configure()
        .build_server(true)
        .file_descriptor_set_path(out_dir.join("photobooth_server_descriptor.bin"))
        .compile_protos(&["./proto/photobooth_server.proto"], &["./proto"])
        .unwrap();
    tonic_build::compile_protos("proto/photobooth_server.proto")?;

    Ok(())
}