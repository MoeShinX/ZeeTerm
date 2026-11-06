//! 敏感信息的加解密：Windows DPAPI（绑定当前用户）。
//!
//! 用 `CryptProtectData` 加一个固定的应用级 entropy，别的程序拿到密文也没法直接
//! 解出来。密文只进 `secret_blob`，绝不出现明文列。

#[cfg(windows)]
use crate::error::reason_keys as rk;
#[cfg(any(windows, not(target_os = "linux")))]
use crate::error::Reason;
use crate::error::ZsshError;

/// 应用级 entropy。这是历史值：当初产品还叫 Z-ssh 时定下的，现在产品叫 ZeeTerm，
/// 这里也保持原样。**永远不要改**——改一个字节，已有的密文就都解不开了。
#[cfg(windows)]
const ENTROPY: &[u8] = b"Z-ssh/v1/secret";

/// 加密一段敏感信息（密码、私钥口令、私钥内容）。
#[cfg(windows)]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: to_u32(plaintext.len())?,
        pbData: plaintext.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: to_u32(ENTROPY.len())?,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: 输入 blob 是本地变量，指针在整个调用期间有效；输出由 DPAPI 分配，
    // 下面用 LocalFree 释放。
    unsafe {
        CryptProtectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(
                    rk::DPAPI_PROTECT_FAILED,
                    format!("CryptProtectData 失败：{err}"),
                )
                .with("cause", &err),
            )
        })?;
    }

    Ok(unsafe { take_blob(output) })
}

/// 解密 [`protect`] 写出来的密文。
#[cfg(windows)]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: to_u32(blob.len())?,
        pbData: blob.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: to_u32(ENTROPY.len())?,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: 同 protect。
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(
                    rk::DPAPI_UNPROTECT_FAILED,
                    format!("CryptUnprotectData 失败：{err}"),
                )
                .with("cause", &err),
            )
        })?;
    }

    Ok(unsafe { take_blob(output) })
}

/// 把 DPAPI 分配的输出 blob 拷成 `Vec`，然后释放。
///
/// # Safety
/// `blob` 必须是 DPAPI 刚分配出来、还没释放的输出。
#[cfg(windows)]
unsafe fn take_blob(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::LocalFree;

    let bytes = if blob.pbData.is_null() {
        Vec::new()
    } else {
        // SAFETY: DPAPI 保证 pbData 指向 cbData 个字节。
        unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec()
    };
    if !blob.pbData.is_null() {
        // SAFETY: 这块内存是 DPAPI 用 LocalAlloc 分配的，只能交给 LocalFree。
        let _ = unsafe { LocalFree(Some(windows::Win32::Foundation::HLOCAL(blob.pbData.cast()))) };
    }
    bytes
}

#[cfg(windows)]
fn to_u32(len: usize) -> Result<u32, ZsshError> {
    u32::try_from(len).map_err(|_| {
        ZsshError::Secret(
            Reason::new(rk::DATA_TOO_LONG, format!("数据太长：{len} 字节")).with("len", len),
        )
    })
}

/// Linux 的资料库密钥管理（数据目录、后台预热、密钥放在哪）；见 `vault.rs`。
#[cfg(target_os = "linux")]
pub use super::vault::{
    init as init_vault, key_state, warm_up as warm_up_vault, KeyBackend, KeyState,
};

/// Linux 没有 DPAPI：用资料库密钥 + XChaCha20-Poly1305，密钥放系统钥匙串（没有钥匙串时放数据目录里的文件）。
#[cfg(target_os = "linux")]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    super::vault::protect(plaintext)
}

#[cfg(target_os = "linux")]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    super::vault::unprotect(blob)
}

/// macOS 的 Keychain 后端尚未实现：明确报错，不静默降级。
#[cfg(all(not(windows), not(target_os = "linux")))]
pub fn protect(_plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    Err(ZsshError::Secret(Reason::text(
        "这个平台还没有实现本机秘密保管",
    )))
}

#[cfg(all(not(windows), not(target_os = "linux")))]
pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    Err(ZsshError::Secret(Reason::text(
        "这个平台还没有实现本机秘密保管",
    )))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_restores_plaintext() {
        let plaintext = "一段带中文的密码 p@ssw0rd".as_bytes();
        let blob = protect(plaintext).expect("加密应该成功");

        assert_ne!(blob, plaintext, "密文不该等于明文");
        assert!(!blob.is_empty(), "密文不该是空的");
        assert_eq!(
            unprotect(&blob).expect("解密应该成功"),
            plaintext,
            "解密之后应该和原文一样"
        );
    }

    #[test]
    fn roundtrip_handles_empty_input() {
        let blob = protect(b"").expect("空数据也能加密");
        assert!(unprotect(&blob).expect("解密应该成功").is_empty());
    }

    #[test]
    fn tampered_blob_is_rejected() {
        let mut blob = protect(b"secret").expect("加密应该成功");
        // 改一个字节：DPAPI 会校验完整性，应该失败而不是解出垃圾。
        if let Some(byte) = blob.first_mut() {
            *byte ^= 0xff;
        }
        assert!(unprotect(&blob).is_err(), "改过的密文不该解得开");
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let plaintext = "a-very-distinctive-password-9f3a";
        let blob = protect(plaintext.as_bytes()).expect("加密应该成功");
        let haystack = String::from_utf8_lossy(&blob);
        assert!(!haystack.contains(plaintext), "密文里不该出现明文片段");
    }
}
