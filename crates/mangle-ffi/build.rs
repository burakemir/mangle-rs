use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    generate_header(&crate_dir);
    compile_c_smoke(&crate_dir);
}

/// Run cbindgen against the crate's Rust sources and write the resulting
/// header to `crates/mangle-ffi/include/mangle.h`. The header is committed;
/// CI diffs against it to catch out-of-sync states.
fn generate_header(crate_dir: &Path) {
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml"))
        .expect("failed to read cbindgen.toml");

    let header_path = crate_dir.join("include").join("mangle.h");

    let bindings = cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen failed to generate bindings");

    // `write_to_file` only writes if the content differs, so a no-op
    // regeneration leaves mtime untouched and doesn't churn cargo's
    // dependency tracking.
    bindings.write_to_file(&header_path);

    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}

/// Compile the C smoke test source into a static archive that the Rust
/// integration test (`tests/c_smoke.rs`) links against. This is how we
/// catch "header doesn't match symbols" bugs: real C code includes the
/// generated header and calls the extern "C" functions.
///
/// The smoke archive's symbols get linked into the cdylib too, but they
/// have internal visibility and are not part of the public ABI.
fn compile_c_smoke(crate_dir: &Path) {
    let smoke_src = crate_dir.join("tests").join("c_smoke").join("main.c");
    if !smoke_src.exists() {
        // Nothing to compile yet (early-build sanity).
        return;
    }

    cc::Build::new()
        .file(&smoke_src)
        .include(crate_dir.join("include"))
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Wno-unused-parameter")
        .compile("mangle_c_smoke");

    println!("cargo:rerun-if-changed=tests/c_smoke/main.c");
    println!("cargo:rerun-if-changed=include/mangle.h");
}
