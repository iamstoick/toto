// `toto-compiler` build script: link the system libcrypto (Wave 1).
//
// Crypto binds the mature system library (OpenSSL 3 on Linux,
// LibreSSL on macOS) with the same load-time link model Phase 5
// established for libc/libm/libpthread: every symbol is known at
// build time, no `dlopen`, no vendored sources, no codegen. The
// wrinkle is that dev symlinks (`libcrypto.so`) are often absent,
// so `-lcrypto` alone fails at link time; instead this script
// symlinks the versioned runtime library it finds into `OUT_DIR`
// and links that (the recorded `DT_NEEDED` is still the SONAME,
// e.g. `libcrypto.so.3`). `TOTO_CRYPTO_LIB_DIR` overrides the
// search list (one directory holding the library file).
fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        return;
    }
    println!("cargo:rerun-if-env-changed=TOTO_CRYPTO_LIB_DIR");
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("TOTO_CRYPTO_LIB_DIR") {
        dirs.push(d);
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("apple-darwin")
        || target.contains("apple-macos")
        || target.contains("darwin")
    {
        dirs.push("/usr/lib".to_string());
    } else if target.contains("aarch64") {
        dirs.push("/usr/lib/aarch64-linux-gnu".to_string());
        dirs.push("/usr/lib64".to_string());
    } else {
        dirs.push("/usr/lib/x86_64-linux-gnu".to_string());
        dirs.push("/usr/lib64".to_string());
    }
    let names: &[&str] = if target.contains("apple") || target.contains("darwin") {
        &["libcrypto.dylib"]
    } else {
        &["libcrypto.so.3", "libcrypto.so.1.1"]
    };
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    for dir in &dirs {
        for name in names {
            let src = std::path::Path::new(dir).join(name);
            if src.is_file() {
                let link_name = if name.ends_with(".dylib") {
                    "libcrypto.dylib"
                } else {
                    "libcrypto.so"
                };
                let dst = std::path::Path::new(&out).join(link_name);
                let _ = std::fs::remove_file(&dst);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dst).expect("symlink libcrypto");
                println!("cargo:rustc-link-search=native={out}");
                println!("cargo:rustc-link-lib=crypto");
                return;
            }
        }
    }
    // No versioned library found: fall back to `-lcrypto` so a
    // dev-installed toolchain (with the unversioned symlink) still
    // links; the failure, if any, names the missing library.
    println!("cargo:rustc-link-lib=crypto");
}
