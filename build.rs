fn main() {
    // Only compile the proto when the grpc feature is enabled.
    #[cfg(feature = "grpc")]
    {
        tonic_build::compile_protos("proto/tick_stream.proto")
            .expect("failed to compile tick_stream.proto");
    }
}
