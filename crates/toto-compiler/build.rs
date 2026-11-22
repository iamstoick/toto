// `toto-compiler` build script: link the system libcrypto (Wave 1),
// libssl (Wave 2, Stage 1: TLS), and the Wave 3 database client
// libraries (sqlite3 everywhere unix; libpq + libmysqlclient on
// Linux only — macOS ships neither as a system library).
//
// Crypto/TLS/DB bind the mature system libraries (OpenSSL 3 on Linux,
// LibreSSL on macOS) with the same load-time link model Phase 5
// established for libc/libm/libpthread: every symbol is known at
// build time, no `dlopen`, no vendored sources, no codegen. The
// wrinkle is that dev symlinks (`libcrypto.so`) are often absent,
// so `-lcrypto`/`-lssl` alone fail at link time; instead this script
// symlinks the versioned runtime libraries it finds into `OUT_DIR`
// and links those (the recorded `DT_NEEDED` is still the SONAME,
// e.g. `libcrypto.so.3`). `TOTO_CRYPTO_LIB_DIR` overrides the
// search list (one directory holding the library files);
// `TOTO_DB_LIB_DIR` does the same for the database client
// libraries (one directory holding the library files).
fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        return;
    }
    println!("cargo:rerun-if-env-changed=TOTO_CRYPTO_LIB_DIR");
    println!("cargo:rerun-if-env-changed=TOTO_DB_LIB_DIR");
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("TOTO_CRYPTO_LIB_DIR") {
        dirs.push(d);
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("apple-darwin")
        || target.contains("apple-macos")
        || target.contains("darwin")
    {
        // System LibreSSL first (present on every macOS with CLT);
        // Homebrew OpenSSL as fallback (same C ABI surface used).
        dirs.push("/usr/lib".to_string());
        dirs.push("/opt/homebrew/opt/openssl@3/lib".to_string());
        dirs.push("/usr/local/opt/openssl@3/lib".to_string());
    } else if target.contains("aarch64") {
        dirs.push("/usr/lib/aarch64-linux-gnu".to_string());
        dirs.push("/lib/aarch64-linux-gnu".to_string());
        dirs.push("/usr/lib64".to_string());
        dirs.push("/lib64".to_string());
        dirs.push("/usr/lib".to_string());
    } else {
        dirs.push("/usr/lib/x86_64-linux-gnu".to_string());
        dirs.push("/lib/x86_64-linux-gnu".to_string());
        dirs.push("/usr/lib64".to_string());
        dirs.push("/lib64".to_string());
        dirs.push("/usr/lib".to_string());
    }
    let names: &[(&str, &str)] = if target.contains("apple") || target.contains("darwin") {
        // (SONAME on disk, link name): LibreSSL ships both libraries
        // as versioned dylibs on every macOS with CLT.
        &[("libcrypto.dylib", "crypto"), ("libssl.dylib", "ssl")]
    } else {
        &[
            ("libcrypto.so.3", "crypto"),
            ("libcrypto.so.1.1", "crypto"),
            ("libssl.so.3", "ssl"),
            ("libssl.so.1.1", "ssl"),
        ]
    };
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    // Database client libraries (Wave 3): same symlink model.
    // sqlite3 is unix-wide (system library on Linux and macOS);
    // libpq + libmysqlclient are Linux-only (macOS ships neither,
    // and the Rust modules are `cfg(target_os = "linux")` to match,
    // so no link directive is emitted for them on Apple targets).
    // `TOTO_DB_LIB_DIR` prefixes the search list (locally extracted
    // .deb contents, CI apt paths, Homebrew cellars).
    let mut db_dirs = Vec::new();
    if let Ok(d) = std::env::var("TOTO_DB_LIB_DIR") {
        for d in d.split(':') {
            db_dirs.push(d.to_string());
        }
    }
    db_dirs.extend(dirs.iter().cloned());
    let is_linux = !target.contains("apple")
        && !target.contains("darwin")
        && (target.contains("linux") || !(target.contains("windows")));
    let mut db_names: Vec<(&str, &str)> = Vec::new();
    if target.contains("apple") || target.contains("darwin") {
        db_names.push(("libsqlite3.dylib", "sqlite3"));
    } else {
        db_names.push(("libsqlite3.so.0", "sqlite3"));
        if is_linux {
            db_names.push(("libpq.so.5", "pq"));
            db_names.push(("libmysqlclient.so.24", "mysqlclient"));
            db_names.push(("libmysqlclient.so.21", "mysqlclient"));
        }
    }
    let mut linked = false;
    // Each library links independently (TLS needs both libssl and
    // libcrypto; either may still resolve on its own).
    for (lib, link) in names {
        for dir in &dirs {
            let src = std::path::Path::new(dir).join(lib);
            if src.is_file() {
                let link_name = if lib.ends_with(".dylib") {
                    (*lib).to_string()
                } else {
                    format!("lib{link}.so")
                };
                let dst = std::path::Path::new(&out).join(&link_name);
                let _ = std::fs::remove_file(&dst);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dst).expect("symlink system lib");
                println!("cargo:rustc-link-search=native={out}");
                println!("cargo:rustc-link-lib={link}");
                linked = true;
                break;
            }
        }
    }
    for (lib, link) in &db_names {
        for dir in &db_dirs {
            let src = std::path::Path::new(dir).join(lib);
            if src.is_file() {
                let link_name = if lib.ends_with(".dylib") {
                    (*lib).to_string()
                } else {
                    format!("lib{link}.so")
                };
                let dst = std::path::Path::new(&out).join(&link_name);
                let _ = std::fs::remove_file(&dst);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dst).expect("symlink system lib");
                println!("cargo:rustc-link-search=native={out}");
                println!("cargo:rustc-link-lib={link}");
                linked = true;
                break;
            }
        }
    }
    if linked {
        return;
    }
    // No versioned library found: fall back to `-lcrypto -lssl` so a
    // dev-installed toolchain (with the unversioned symlinks) still
    // links; the failure, if any, names the missing library.
    println!("cargo:rustc-link-lib=crypto");
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=sqlite3");
    if is_linux {
        println!("cargo:rustc-link-lib=pq");
        println!("cargo:rustc-link-lib=mysqlclient");
    }
}
