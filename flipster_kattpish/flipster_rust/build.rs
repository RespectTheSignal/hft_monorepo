fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .map_err(|e| format!("failed to locate vendored protoc: {}", e))?;
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["schemas/order_processor.proto"], &["schemas"])?;

    println!("cargo:rerun-if-changed=schemas/order_processor.proto");
    Ok(())
}
