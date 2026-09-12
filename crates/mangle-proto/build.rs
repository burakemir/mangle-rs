// Generate the buffa message types and connectrpc service stubs for the
// canonical Mangle protos. `protoc` is provided by protoc-bin-vendored so
// the build is hermetic (no system protoc required).

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("protoc binary");
    // connectrpc-build reads PROTOC when resolving the compiler. Build
    // scripts are single-threaded per package, so set_var is fine here.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    connectrpc_build::Config::new()
        .files(&["proto/mangle/value.proto", "proto/mangle/service.proto"])
        .includes(&["proto/"])
        .include_file("_connectrpc.rs")
        .compile()
        .expect("connectrpc codegen");
}
