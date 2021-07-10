pub mod pb {
    tonic::include_proto!("fdb.grpc"); // The string specified here must match the proto package name
}

pub mod server;
