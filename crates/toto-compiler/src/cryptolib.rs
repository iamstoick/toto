// `cryptolib` — OpenSSL/LibreSSL-backed crypto for Enterprise Wave 1.
//
// Toto calls these through `toto_rt_crypto_*` foreign functions
// (declared in `gzv.std/crypto`, allowlisted in `ffi.rs`, refused by
// the JIT like the net surface). The primitives themselves are NOT
// implemented here: every digest byte, tag, ciphertext, signature,
// random byte, and certificate verdict comes out of the linked
// system libcrypto (OpenSSL 3 on Linux, LibreSSL on macOS — the
// same load-time link model as libc/libm; see `docs/ffi.md` and the
// `toto-compiler` build script). This module is handle/bytes
// plumbing only: one-shot calls (no retained contexts, no key
// handles — rooting covers the call), opaque pointers that never
// cross into Toto (Toto sees `Array<int>` bytes and `int` codes),
// and `Result<_, String>` errors. Unix-only like the net surface;
// Windows gets clean check-time `E0301` (see `POSIX_ONLY`).
//
// Only long-stable APIs are used (classic `EVP_Digest*`, `HMAC()`,
// `EVP_aes_256_gcm`, `EC_KEY`/`ECDSA_sign`/`ECDSA_verify`,
// `RAND_bytes`, `X509_STORE` verify, `OpenSSL_version`) — present
// in OpenSSL 1.1.1+/3.x and LibreSSL alike. No OSSL-3-only,
// provider-side, or struct-by-value APIs anywhere.

#[cfg(unix)]
pub use inner::*;
#[cfg(unix)]
mod inner {
    use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};

    // Opaque OpenSSL types: pointers only, never constructed here.
    // `Bio`/`X509`/`X509Store`/`EvpPkey` are `pub(crate)` — the Wave 2
    // TLS bridge (`tlslib`) shares these declaration sites instead of
    // redeclaring the same C symbols (which would trip
    // `clashing_extern_declarations` and split the contract in two).
    enum EvpMd {}
    enum EvpMdCtx {}
    enum EvpCipher {}
    enum EvpCipherCtx {}
    enum EcKey {}
    enum EcGroup {}
    enum EcPoint {}
    enum BigNum {}
    pub(crate) enum Bio {}
    pub(crate) enum X509 {}
    pub(crate) enum X509Store {}
    pub(crate) enum EvpPkey {}
    enum X509StoreCtx {}

    #[link(name = "crypto")]
    unsafe extern "C" {
        // -- Digests --
        fn EVP_sha256() -> *const EvpMd;
        fn EVP_sha512() -> *const EvpMd;
        fn EVP_MD_CTX_new() -> *mut EvpMdCtx;
        fn EVP_MD_CTX_free(ctx: *mut EvpMdCtx);
        fn EVP_DigestInit_ex(ctx: *mut EvpMdCtx, md: *const EvpMd, impl_: *const c_void) -> c_int;
        fn EVP_DigestUpdate(ctx: *mut EvpMdCtx, data: *const c_void, len: usize) -> c_int;
        fn EVP_DigestFinal_ex(ctx: *mut EvpMdCtx, out: *mut c_uchar, outl: *mut c_uint) -> c_int;
        // -- HMAC (one-shot) --
        fn HMAC(
            evp_md: *const EvpMd,
            key: *const c_void,
            key_len: c_int,
            d: *const c_uchar,
            n: usize,
            md: *mut c_uchar,
            md_len: *mut c_uint,
        ) -> *mut c_uchar;
    }

    /// Drain the OpenSSL error queue into one line (`ctx` names the
    /// call); empty queue falls back to the context alone.
    fn ossl_err(ctx: &str) -> String {
        unsafe extern "C" {
            fn ERR_get_error() -> u64;
            fn ERR_error_string_n(e: u64, buf: *mut c_char, len: usize);
        }
        let mut parts = Vec::new();
        for _ in 0..8 {
            let code = unsafe { ERR_get_error() };
            if code == 0 {
                break;
            }
            let mut buf = vec![0 as c_char; 256];
            unsafe { ERR_error_string_n(code, buf.as_mut_ptr(), buf.len()) };
            let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            parts.push(s);
        }
        if parts.is_empty() {
            ctx.to_string()
        } else {
            format!("{ctx}: {}", parts.join("; "))
        }
    }

    fn digest(md: *const EvpMd, data: &[u8], ctx: &str) -> Result<Vec<u8>, String> {
        unsafe {
            let c = EVP_MD_CTX_new();
            if c.is_null() {
                return Err(format!("{ctx}: context allocation failed"));
            }
            let r = (|| {
                if EVP_DigestInit_ex(c, md, std::ptr::null()) != 1 {
                    return Err(ossl_err(ctx));
                }
                let p = if data.is_empty() {
                    std::ptr::null()
                } else {
                    data.as_ptr() as *const c_void
                };
                if EVP_DigestUpdate(c, p, data.len()) != 1 {
                    return Err(ossl_err(ctx));
                }
                let mut out = vec![0u8; 64];
                let mut outl: c_uint = 0;
                if EVP_DigestFinal_ex(c, out.as_mut_ptr(), &mut outl) != 1 {
                    return Err(ossl_err(ctx));
                }
                out.truncate(outl as usize);
                Ok(out)
            })();
            EVP_MD_CTX_free(c);
            r
        }
    }

    pub fn sha256(data: &[u8]) -> Result<Vec<u8>, String> {
        unsafe { digest(EVP_sha256(), data, "sha256") }
    }

    pub fn sha512(data: &[u8]) -> Result<Vec<u8>, String> {
        unsafe { digest(EVP_sha512(), data, "sha512") }
    }

    fn hmac_once(md: *const EvpMd, key: &[u8], data: &[u8], ctx: &str) -> Result<Vec<u8>, String> {
        if key.len() > c_int::MAX as usize {
            return Err(format!("{ctx}: key too long"));
        }
        unsafe {
            let mut out = vec![0u8; 64];
            let mut outl: c_uint = 0;
            let p = if data.is_empty() {
                std::ptr::null()
            } else {
                data.as_ptr()
            };
            let k = if key.is_empty() {
                std::ptr::null()
            } else {
                key.as_ptr() as *const c_void
            };
            let r = HMAC(
                md,
                k,
                key.len() as c_int,
                p,
                data.len(),
                out.as_mut_ptr(),
                &mut outl,
            );
            if r.is_null() {
                return Err(ossl_err(ctx));
            }
            out.truncate(outl as usize);
            Ok(out)
        }
    }

    pub fn hmac256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
        unsafe { hmac_once(EVP_sha256(), key, data, "hmac_sha256") }
    }

    pub fn hmac512(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
        unsafe { hmac_once(EVP_sha512(), key, data, "hmac_sha512") }
    }

    // -- AES-256-GCM (one-shot) --
    const GCM_TAG_LEN: usize = 16;
    // `EVP_CTRL_GCM_GET_TAG` / `EVP_CTRL_GCM_SET_TAG` (stable numbers).
    const EVP_CTRL_GCM_GET_TAG: c_int = 0x10;
    const EVP_CTRL_GCM_SET_TAG: c_int = 0x11;

    unsafe extern "C" {
        fn EVP_aes_256_gcm() -> *const EvpCipher;
        fn EVP_CIPHER_CTX_new() -> *mut EvpCipherCtx;
        fn EVP_CIPHER_CTX_free(ctx: *mut EvpCipherCtx);
        fn EVP_CIPHER_CTX_ctrl(
            ctx: *mut EvpCipherCtx,
            type_: c_int,
            arg: c_int,
            ptr: *mut c_void,
        ) -> c_int;
        fn EVP_EncryptInit_ex(
            ctx: *mut EvpCipherCtx,
            cipher: *const EvpCipher,
            impl_: *const c_void,
            key: *const c_uchar,
            iv: *const c_uchar,
        ) -> c_int;
        fn EVP_EncryptUpdate(
            ctx: *mut EvpCipherCtx,
            out: *mut c_uchar,
            outl: *mut c_int,
            in_: *const c_uchar,
            inl: c_int,
        ) -> c_int;
        fn EVP_EncryptFinal_ex(
            ctx: *mut EvpCipherCtx,
            out: *mut c_uchar,
            outl: *mut c_int,
        ) -> c_int;
        fn EVP_DecryptInit_ex(
            ctx: *mut EvpCipherCtx,
            cipher: *const EvpCipher,
            impl_: *const c_void,
            key: *const c_uchar,
            iv: *const c_uchar,
        ) -> c_int;
        fn EVP_DecryptUpdate(
            ctx: *mut EvpCipherCtx,
            out: *mut c_uchar,
            outl: *mut c_int,
            in_: *const c_uchar,
            inl: c_int,
        ) -> c_int;
        fn EVP_DecryptFinal_ex(
            ctx: *mut EvpCipherCtx,
            out: *mut c_uchar,
            outl: *mut c_int,
        ) -> c_int;
    }

    fn check_len(what: &str, v: &[u8], want: usize) -> Result<(), String> {
        if v.len() != want {
            return Err(format!("{what}: need {want} bytes, got {}", v.len()));
        }
        Ok(())
    }

    /// AES-256-GCM encrypt: output is `ciphertext || tag(16)`.
    pub fn aead_encrypt(
        key: &[u8],
        nonce: &[u8],
        pt: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, String> {
        check_len("aead_encrypt key", key, 32)?;
        check_len("aead_encrypt nonce", nonce, 12)?;
        if pt.len() > c_int::MAX as usize || aad.len() > c_int::MAX as usize {
            return Err("aead_encrypt: input too long".to_string());
        }
        unsafe {
            let c = EVP_CIPHER_CTX_new();
            if c.is_null() {
                return Err("aead_encrypt: context allocation failed".to_string());
            }
            let r = (|| {
                if EVP_EncryptInit_ex(
                    c,
                    EVP_aes_256_gcm(),
                    std::ptr::null(),
                    key.as_ptr(),
                    nonce.as_ptr(),
                ) != 1
                {
                    return Err(ossl_err("aead_encrypt init"));
                }
                let mut outl: c_int = 0;
                if !aad.is_empty()
                    && EVP_EncryptUpdate(
                        c,
                        std::ptr::null_mut(),
                        &mut outl,
                        aad.as_ptr(),
                        aad.len() as c_int,
                    ) != 1
                {
                    return Err(ossl_err("aead_encrypt aad"));
                }
                let mut ct = vec![0u8; pt.len() + 16];
                let mut n: c_int = 0;
                let p = if pt.is_empty() {
                    std::ptr::null()
                } else {
                    pt.as_ptr()
                };
                if EVP_EncryptUpdate(c, ct.as_mut_ptr(), &mut n, p, pt.len() as c_int) != 1 {
                    return Err(ossl_err("aead_encrypt data"));
                }
                let mut total = n as usize;
                if EVP_EncryptFinal_ex(c, ct.as_mut_ptr().add(total), &mut n) != 1 {
                    return Err(ossl_err("aead_encrypt final"));
                }
                total += n as usize;
                let mut tag = [0u8; GCM_TAG_LEN];
                if EVP_CIPHER_CTX_ctrl(
                    c,
                    EVP_CTRL_GCM_GET_TAG,
                    GCM_TAG_LEN as c_int,
                    tag.as_mut_ptr() as *mut c_void,
                ) != 1
                {
                    return Err(ossl_err("aead_encrypt tag"));
                }
                ct.truncate(total);
                ct.extend_from_slice(&tag);
                Ok(ct)
            })();
            EVP_CIPHER_CTX_free(c);
            r
        }
    }

    /// AES-256-GCM decrypt: input is `ciphertext || tag(16)`; a bad
    /// tag (or bad lengths) is `Ok(false)`-style failure expressed
    /// here as `Err("authentication failed")` — the Toto wrapper
    /// reports it as a value-level `Err`, never a panic.
    pub fn aead_decrypt(
        key: &[u8],
        nonce: &[u8],
        cttag: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, String> {
        check_len("aead_decrypt key", key, 32)?;
        check_len("aead_decrypt nonce", nonce, 12)?;
        if cttag.len() < GCM_TAG_LEN {
            return Err("aead_decrypt: input shorter than the 16-byte tag".to_string());
        }
        if aad.len() > c_int::MAX as usize || cttag.len() > c_int::MAX as usize {
            return Err("aead_decrypt: input too long".to_string());
        }
        let (ct, tag) = cttag.split_at(cttag.len() - GCM_TAG_LEN);
        unsafe {
            let c = EVP_CIPHER_CTX_new();
            if c.is_null() {
                return Err("aead_decrypt: context allocation failed".to_string());
            }
            let r = (|| {
                if EVP_DecryptInit_ex(
                    c,
                    EVP_aes_256_gcm(),
                    std::ptr::null(),
                    key.as_ptr(),
                    nonce.as_ptr(),
                ) != 1
                {
                    return Err(ossl_err("aead_decrypt init"));
                }
                let mut outl: c_int = 0;
                if !aad.is_empty()
                    && EVP_DecryptUpdate(
                        c,
                        std::ptr::null_mut(),
                        &mut outl,
                        aad.as_ptr(),
                        aad.len() as c_int,
                    ) != 1
                {
                    return Err(ossl_err("aead_decrypt aad"));
                }
                if EVP_CIPHER_CTX_ctrl(
                    c,
                    EVP_CTRL_GCM_SET_TAG,
                    GCM_TAG_LEN as c_int,
                    tag.as_ptr() as *mut c_void,
                ) != 1
                {
                    return Err(ossl_err("aead_decrypt set tag"));
                }
                let mut pt = vec![0u8; ct.len() + 16];
                let mut n: c_int = 0;
                let p = if ct.is_empty() {
                    std::ptr::null()
                } else {
                    ct.as_ptr()
                };
                if EVP_DecryptUpdate(c, pt.as_mut_ptr(), &mut n, p, ct.len() as c_int) != 1 {
                    return Err(ossl_err("aead_decrypt data"));
                }
                let mut total = n as usize;
                // Tag mismatch fails HERE (returns 0): authentication
                // failure, reported as a value error.
                if EVP_DecryptFinal_ex(c, pt.as_mut_ptr().add(total), &mut n) != 1 {
                    return Err("aead_decrypt: authentication failed".to_string());
                }
                total += n as usize;
                pt.truncate(total);
                Ok(pt)
            })();
            EVP_CIPHER_CTX_free(c);
            r
        }
    }

    // -- ECDSA P-256 (sign/verify) + CSPRNG --
    // `NID_X9_62_prime256v1` (stable number, both libraries).
    const NID_P256: c_int = 415;
    const POINT_CONVERSION_UNCOMPRESSED: u8 = 4;

    unsafe extern "C" {
        fn EC_KEY_new_by_curve_name(nid: c_int) -> *mut EcKey;
        fn EC_KEY_free(key: *mut EcKey);
        fn EC_KEY_generate_key(key: *mut EcKey) -> c_int;
        fn EC_KEY_get0_private_key(key: *const EcKey) -> *const BigNum;
        fn EC_KEY_get0_public_key(key: *const EcKey) -> *const EcPoint;
        fn EC_KEY_get0_group(key: *const EcKey) -> *const EcGroup;
        fn EC_KEY_set_private_key(key: *mut EcKey, prv: *const BigNum) -> c_int;
        fn EC_KEY_set_public_key(key: *mut EcKey, pub_: *const EcPoint) -> c_int;
        fn EC_POINT_new(group: *const EcGroup) -> *mut EcPoint;
        fn EC_POINT_free(point: *mut EcPoint);
        fn EC_POINT_point2oct(
            group: *const EcGroup,
            p: *const EcPoint,
            form: u8,
            buf: *mut c_uchar,
            len: usize,
            ctx: *mut c_void,
        ) -> usize;
        fn EC_POINT_oct2point(
            group: *const EcGroup,
            p: *mut EcPoint,
            buf: *const c_uchar,
            len: usize,
            ctx: *mut c_void,
        ) -> c_int;
        fn BN_bin2bn(s: *const c_uchar, len: c_int, ret: *mut BigNum) -> *mut BigNum;
        fn BN_free(a: *mut BigNum);
        // NOTE: `BN_num_bytes` is a C macro in OpenSSL 3 (not an
        // exported symbol), so this bridge uses `BN_num_bits` and
        // rounds up — identical semantics, linkable everywhere.
        fn BN_num_bits(a: *const BigNum) -> c_int;
        fn BN_bn2bin(a: *const BigNum, to: *mut c_uchar) -> c_int;
        fn ECDSA_size(eckey: *const EcKey) -> c_int;
        fn ECDSA_sign(
            type_: c_int,
            dgst: *const c_uchar,
            dgstlen: c_int,
            sig: *mut c_uchar,
            siglen: *mut c_uint,
            eckey: *mut EcKey,
        ) -> c_int;
        fn ECDSA_verify(
            type_: c_int,
            dgst: *const c_uchar,
            dgstlen: c_int,
            sig: *const c_uchar,
            siglen: c_int,
            eckey: *mut EcKey,
        ) -> c_int;
        fn RAND_bytes(buf: *mut c_uchar, num: c_int) -> c_int;
    }

    /// Fresh P-256 keypair: `priv(32) || pub(64)` (`x || y`, no
    /// `0x04` prefix — the wrapper splits this into a struct).
    pub fn sign_keygen() -> Result<Vec<u8>, String> {
        unsafe {
            let k = EC_KEY_new_by_curve_name(NID_P256);
            if k.is_null() {
                return Err(ossl_err("sign_keygen alloc"));
            }
            let r = (|| {
                if EC_KEY_generate_key(k) != 1 {
                    return Err(ossl_err("sign_keygen generate"));
                }
                let priv_bn = EC_KEY_get0_private_key(k);
                if priv_bn.is_null() {
                    return Err("sign_keygen: no private value".to_string());
                }
                let nbytes = (BN_num_bits(priv_bn) + 7) / 8;
                if nbytes <= 0 || nbytes > 32 {
                    return Err("sign_keygen: bad private size".to_string());
                }
                let mut tmp = vec![0u8; nbytes as usize];
                if BN_bn2bin(priv_bn, tmp.as_mut_ptr()) as usize != tmp.len() {
                    return Err(ossl_err("sign_keygen export"));
                }
                let mut priv32 = vec![0u8; 32 - tmp.len()];
                priv32.extend_from_slice(&tmp);
                let group = EC_KEY_get0_group(k);
                let pub_pt = EC_KEY_get0_public_key(k);
                if group.is_null() || pub_pt.is_null() {
                    return Err("sign_keygen: no public value".to_string());
                }
                let mut oct = vec![0u8; 65];
                let n = EC_POINT_point2oct(
                    group,
                    pub_pt,
                    POINT_CONVERSION_UNCOMPRESSED,
                    oct.as_mut_ptr(),
                    65,
                    std::ptr::null_mut(),
                );
                if n != 65 || oct[0] != 4 {
                    return Err(ossl_err("sign_keygen encode"));
                }
                let mut out = priv32;
                out.extend_from_slice(&oct[1..]);
                Ok(out)
            })();
            EC_KEY_free(k);
            r
        }
    }

    /// Rebuild a key from a raw 32-byte scalar (private side only).
    /// Safe wrapper: the slice length is checked, every pointer is
    /// null-checked before use, and nothing is retained past return.
    fn key_from_priv(secret: &[u8]) -> Result<*mut EcKey, String> {
        // SAFETY: length-checked slice; all foreign pointers are
        // null-checked; freed on every path; never retained.
        unsafe {
            check_len("sign key", secret, 32)?;
            let k = EC_KEY_new_by_curve_name(NID_P256);
            if k.is_null() {
                return Err(ossl_err("sign alloc"));
            }
            let bn = BN_bin2bn(secret.as_ptr(), 32, std::ptr::null_mut());
            if bn.is_null() {
                EC_KEY_free(k);
                return Err(ossl_err("sign scalar"));
            }
            let ok = EC_KEY_set_private_key(k, bn) == 1;
            BN_free(bn);
            if !ok {
                EC_KEY_free(k);
                return Err(ossl_err("sign set key"));
            }
            Ok(k)
        }
    }

    /// Rebuild a verify-only key from a raw 64-byte `x || y`.
    /// Safe wrapper: same contract as `key_from_priv`.
    fn key_from_pub(public: &[u8]) -> Result<*mut EcKey, String> {
        // SAFETY: length-checked slice; all foreign pointers are
        // null-checked; freed on every path; never retained.
        unsafe {
            check_len("verify key", public, 64)?;
            let k = EC_KEY_new_by_curve_name(NID_P256);
            if k.is_null() {
                return Err(ossl_err("verify alloc"));
            }
            let r = (|| {
                let group = EC_KEY_get0_group(k);
                if group.is_null() {
                    return Err(ossl_err("verify group"));
                }
                let pt = EC_POINT_new(group);
                if pt.is_null() {
                    return Err(ossl_err("verify point alloc"));
                }
                let r = (|| {
                    let mut oct = [4u8; 65];
                    oct[1..].copy_from_slice(public);
                    if EC_POINT_oct2point(group, pt, oct.as_ptr(), 65, std::ptr::null_mut()) != 1 {
                        return Err("verify: not a P-256 point".to_string());
                    }
                    if EC_KEY_set_public_key(k, pt) != 1 {
                        return Err(ossl_err("verify set key"));
                    }
                    Ok(())
                })();
                EC_POINT_free(pt);
                r
            })();
            if r.is_err() {
                EC_KEY_free(k);
            }
            r.map(|()| k)
        }
    }

    /// Sign (SHA-256 of `msg`, DER signature).
    pub fn sign_sign(secret: &[u8], msg: &[u8]) -> Result<Vec<u8>, String> {
        let dgst = sha256(msg).map_err(|e| format!("sign digest: {e}"))?;
        unsafe {
            let k = key_from_priv(secret)?;
            let r = (|| {
                let size = ECDSA_size(k);
                if size <= 0 || size > 256 {
                    return Err(ossl_err("sign size"));
                }
                let mut sig = vec![0u8; size as usize];
                let mut siglen: c_uint = 0;
                if ECDSA_sign(
                    0,
                    dgst.as_ptr(),
                    dgst.len() as c_int,
                    sig.as_mut_ptr(),
                    &mut siglen,
                    k,
                ) != 1
                {
                    return Err(ossl_err("sign"));
                }
                sig.truncate(siglen as usize);
                Ok(sig)
            })();
            EC_KEY_free(k);
            r
        }
    }

    /// Verify: `Ok(true)` valid, `Ok(false)` bad math, `Err` on bad
    /// inputs (wrong key/signature shape, off-curve point).
    pub fn sign_verify(public: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String> {
        if sig.len() > c_int::MAX as usize {
            return Err("verify: signature too long".to_string());
        }
        let dgst = sha256(msg).map_err(|e| format!("verify digest: {e}"))?;
        unsafe {
            let k = key_from_pub(public)?;
            let r = ECDSA_verify(
                0,
                dgst.as_ptr(),
                dgst.len() as c_int,
                sig.as_ptr(),
                sig.len() as c_int,
                k,
            );
            EC_KEY_free(k);
            match r {
                1 => Ok(true),
                0 => Ok(false),
                // Fixed string (no library queue text): malformed
                // signatures are pinned in goldens, and queue wording
                // differs between OpenSSL and LibreSSL.
                _ => Err("verify: bad signature encoding".to_string()),
            }
        }
    }

    pub fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
        if n == 0 || n > 65536 {
            return Err(format!("random: need 1-65536 bytes, got {n}"));
        }
        unsafe {
            let mut out = vec![0u8; n];
            if RAND_bytes(out.as_mut_ptr(), n as c_int) != 1 {
                return Err(ossl_err("random"));
            }
            Ok(out)
        }
    }

    // -- X.509 chain verification --
    // `pub(crate)` items are the shared declaration site for the
    // Wave 2 TLS bridge (see the opaque-type note above).
    unsafe extern "C" {
        fn BIO_new_mem_buf(buf: *const c_void, len: c_int) -> *mut Bio;
        fn BIO_free(b: *mut Bio) -> c_int;
        fn PEM_read_bio_X509(
            bp: *mut Bio,
            x: *mut *mut X509,
            cb: Option<unsafe extern "C" fn(*mut c_char, c_int, c_int, *mut c_void) -> c_int>,
            u: *mut c_void,
        ) -> *mut X509;
        pub(crate) fn X509_free(x: *mut X509);
        fn X509_STORE_new() -> *mut X509Store;
        fn X509_STORE_free(s: *mut X509Store);
        pub(crate) fn X509_STORE_add_cert(s: *mut X509Store, x: *mut X509) -> c_int;
        fn X509_STORE_CTX_new() -> *mut X509StoreCtx;
        fn X509_STORE_CTX_free(c: *mut X509StoreCtx);
        fn X509_STORE_CTX_init(
            ctx: *mut X509StoreCtx,
            store: *mut X509Store,
            x509: *mut X509,
            chain: *mut c_void,
        ) -> c_int;
        fn X509_verify_cert(ctx: *mut X509StoreCtx) -> c_int;
        fn OpenSSL_version(t: c_int) -> *const c_char;
        pub(crate) fn PEM_read_bio_PrivateKey(
            bp: *mut Bio,
            x: *mut *mut EvpPkey,
            cb: Option<unsafe extern "C" fn(*mut c_char, c_int, c_int, *mut c_void) -> c_int>,
            u: *mut c_void,
        ) -> *mut EvpPkey;
        pub(crate) fn EVP_PKEY_free(k: *mut EvpPkey);
    }

    /// Safe wrapper: the slice outlives the call, the BIO is freed
    /// on every path, and the parsed cert is null-checked. Shared
    /// with the TLS bridge (`pub(crate)` — same declaration-site
    /// rule as the types above).
    pub(crate) fn pem_to_x509(pem: &str, what: &str) -> Result<*mut X509, String> {
        // SAFETY: borrowed slice outlives the call; BIO freed on
        // every path; result null-checked; never retained.
        unsafe {
            if pem.len() > c_int::MAX as usize {
                return Err(format!("{what}: PEM too long"));
            }
            let bio = BIO_new_mem_buf(pem.as_ptr() as *const c_void, pem.len() as c_int);
            if bio.is_null() {
                return Err(format!("{what}: BIO allocation failed"));
            }
            let x = PEM_read_bio_X509(bio, std::ptr::null_mut(), None, std::ptr::null_mut());
            BIO_free(bio);
            // Fixed string (no library queue text): certificate fixtures
            // and negative tests pin this in goldens, and queue wording
            // differs between OpenSSL and LibreSSL.
            if x.is_null() {
                return Err(format!("{what}: unparseable PEM"));
            }
            Ok(x)
        }
    }

    /// Parse one PEM private key (same contract as `pem_to_x509`).
    /// Shared with the TLS bridge (`pub(crate)`).
    pub(crate) fn pem_to_pkey(pem: &str, what: &str) -> Result<*mut EvpPkey, String> {
        // SAFETY: borrowed slice outlives the call; BIO freed on
        // every path; result null-checked; never retained.
        unsafe {
            if pem.len() > c_int::MAX as usize {
                return Err(format!("{what}: PEM too long"));
            }
            let bio = BIO_new_mem_buf(pem.as_ptr() as *const c_void, pem.len() as c_int);
            if bio.is_null() {
                return Err(format!("{what}: BIO allocation failed"));
            }
            let k = PEM_read_bio_PrivateKey(bio, std::ptr::null_mut(), None, std::ptr::null_mut());
            BIO_free(bio);
            if k.is_null() {
                return Err(format!("{what}: unparseable PEM"));
            }
            Ok(k)
        }
    }

    /// Verify `leaf_pem` chains to the `ca_pem` trust root:
    /// `Ok(true)` verifies, `Ok(false)` does not (expired, wrong
    /// issuer, ...), `Err` on unparseable input.
    pub fn x509_verify(leaf_pem: &str, ca_pem: &str) -> Result<bool, String> {
        unsafe {
            let leaf = pem_to_x509(leaf_pem, "x509 leaf")?;
            let r = (|| {
                let ca = pem_to_x509(ca_pem, "x509 ca")?;
                let r = (|| {
                    let store = X509_STORE_new();
                    if store.is_null() {
                        return Err("x509: store allocation failed".to_string());
                    }
                    let r = (|| {
                        // add_cert bumps the refcount on success; the
                        // parsed `ca` is still ours to free.
                        if X509_STORE_add_cert(store, ca) != 1 {
                            return Err(ossl_err("x509 trust add"));
                        }
                        let ctx = X509_STORE_CTX_new();
                        if ctx.is_null() {
                            return Err("x509: context allocation failed".to_string());
                        }
                        let r = (|| {
                            if X509_STORE_CTX_init(ctx, store, leaf, std::ptr::null_mut()) != 1 {
                                return Err(ossl_err("x509 context init"));
                            }
                            match X509_verify_cert(ctx) {
                                1 => Ok(true),
                                0 => Ok(false),
                                _ => Err(ossl_err("x509 verify")),
                            }
                        })();
                        X509_STORE_CTX_free(ctx);
                        r
                    })();
                    X509_STORE_free(store);
                    r
                })();
                X509_free(ca);
                r
            })();
            X509_free(leaf);
            r
        }
    }

    pub fn openssl_version() -> String {
        unsafe {
            let p = OpenSSL_version(0);
            if p.is_null() {
                return "unknown".to_string();
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
