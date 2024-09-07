fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "rpc-codegen")]
    let builder = tonic_build::configure()
        .build_client(false)
        .out_dir("src/rpc/codegen/");

    #[cfg(all(feature = "rpc-codegen", not(feature = "grpc-service")))]
    let builder = builder.build_server(false);

    #[cfg(feature = "rpc-codegen")]
    builder.compile(&["simulation.proto"], &["src/rpc/api/"])?;

    Ok(())
}
