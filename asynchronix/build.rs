fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(asynchronix_grpc_codegen)]
    tonic_build::configure()
        .build_client(false)
        .out_dir("src/grpc/codegen/")
        .compile(&["simulation.proto"], &["src/grpc/api/"])?;

    Ok(())
}
