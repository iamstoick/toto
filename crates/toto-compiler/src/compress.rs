// `compress` — gzip backend for Enterprise Wave 1 serialization.
//
// The `flate2` crate (pure-Rust DEFLATE via `miniz_oxide`: no C
// code, no system library, identical bytes on every platform)
// behind two one-shot byte functions. Toto calls these through
// `toto_rt_compress_gzip` / `toto_rt_decompress_gzip` (allowlisted
// in `ffi.rs`, refused by the JIT like the other byte-array
// surfaces). Format choice: gzip (RFC 1952) with default
// compression — interop is proven against the system `gzip`
// utility, not against our own bytes. Decompression failures are
// a fixed string (never library wording), so goldens hold on every
// platform.

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::io::{Read, Write};

/// Compress `data` to a gzip stream (RFC 1952, default level).
pub fn gzip_compress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)
        .map_err(|e| format!("compress: write failed: {e}"))?;
    enc.finish()
        .map_err(|e| format!("compress: finish failed: {e}"))
}

/// Decompress a gzip stream. Any failure (truncated, corrupt,
// non-gzip input) is one fixed string — no library wording, so
// error goldens are platform-stable.
pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|_| "decompress: invalid gzip data".to_string())?;
    Ok(out)
}
