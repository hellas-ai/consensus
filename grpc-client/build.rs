fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile proto files to OUT_DIR (used by tonic::include_proto!)
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/minimmit/v1/common.proto",
                "proto/minimmit/v1/transaction.proto",
                "proto/minimmit/v1/account.proto",
                "proto/minimmit/v1/block.proto",
                "proto/minimmit/v1/node.proto",
                "proto/minimmit/v1/subscription.proto",
                "proto/minimmit/v1/admin.proto",
            ],
            &["proto"],
        )?;

    // Tell Cargo to rerun if proto files change
    println!("cargo:rerun-if-changed=proto/");

    Ok(())
}
