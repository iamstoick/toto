//! Embedded `gzv.std` sources (Phase 5, Stage 2).
//!
//! The standard library ships as Toto source in `stdlib/*.gzv` at the
//! repo root. It is embedded here so every `toto` binary resolves
//! `import "gzv.std/<mod>.gzv"` without a source tree present. Import
//! paths resolve canonically (see `project::resolve`); the CLI read
//! callbacks serve these from this table first, then fall back to
//! disk (run/build/test), which lets stdlib developers iterate with a
//! local `gzv.std/` overlay directory.
//!
//! Display paths stay `gzv.std/<mod>.gzv`, so diagnostics point at a
//! stable module name.

macro_rules! std_module {
    ($name:literal) => {
        (
            concat!("gzv.std/", $name, ".gzv"),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../stdlib/",
                $name,
                ".gzv"
            )),
        )
    };
}

/// `(canonical path, source)` for every stdlib module.
pub const MODULES: &[(&str, &str)] = &[
    std_module!("io"),
    std_module!("fs"),
    std_module!("strings"),
    std_module!("collections"),
    std_module!("math"),
    std_module!("time"),
    std_module!("json"),
    std_module!("ffi"),
    std_module!("ffi_thread"),
    std_module!("net"),
    std_module!("crypto"),
    std_module!("compress"),
    std_module!("toml"),
    std_module!("yaml"),
    std_module!("proto"),
];

/// Embedded source for a canonical `gzv.std/...` path, if known.
pub fn get(path: &str) -> Option<&'static str> {
    MODULES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, src)| *src)
}
