fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(nexosim_grpc_codegen)]
    tonic_build::configure()
        .build_client(false)
        .out_dir("src/grpc/codegen/")
        .compile_protos(&["simulation.proto"], &["src/grpc/api/"])?;

    Ok(())
}
