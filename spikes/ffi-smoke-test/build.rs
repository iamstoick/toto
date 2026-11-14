// Build the single C file under test with Same-flags-as-CI warnings.
// Recorded toolchain: gcc 15.2.0 (Ubuntu) via the `cc` crate driver.
// `-Wall -Wextra` are on; `-Werror` is deliberately NOT set so a new
// compiler's new warnings stay visible (and must be justified)
// instead of failing the build opaquely. The build log is checked
// for warnings when the report is written.
fn main() {
    cc::Build::new()
        .file("csrc/toto_ffi_test.c")
        .flag("-Wall")
        .flag("-Wextra")
        .flag("-std=c11")
        .compile("toto_ffi_test");
    println!("cargo:rerun-if-changed=csrc/toto_ffi_test.c");
}
