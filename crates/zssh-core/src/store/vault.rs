//! Linux 本机秘密：随机资料库密钥 + XChaCha20-Poly1305。
//!
//! Windows 继续用 DPAPI；Linux 没有等价的内核用户绑定能力，所以按方案 3.2：
//! - 生成 32 字节随机资料库密钥；
//! - 密钥只放 Secret Service（GNOME Keyring / KWallet）；
//! - 密文留在 SQLite 的 `secret_blob` 列，格式带 magic、版本和随机 nonce。
//!
//! Secret Service 不可用时**明确报错**，不会把密钥写到数据库旁边，也不会退回明文。
//! 单元测试用固定测试密钥验证加解密本身；生产路径始终走 Secret Service。

#[cfg(not(test))]
use std::sync::Mutex;
#[cfg(not(test))]
use std::time::Duration;

#[cfg(not(test))]
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngExt as _;
use zeroize::Zeroizing;

use crate::error::{reason_keys as rk, Reason, ZsshError};

/// 密文头：8 字节 magic（含格式版本）。
const MAGIC: &[u8; 8] = b"ZSVLT01\n";
/// XChaCha20-Poly1305 的 nonce 长度。
const NONCE_LEN: usize = 24;
/// 认证标签长度。
const TAG_LEN: usize = 16;
/// Secret Service 里的产品命名空间与条目名。
#[cfg(not(test))]
const SERVICE: &str = "app.zeeterm";
#[cfg(not(test))]
const ENTRY: &str = "local-vault-key";
/// 等 Secret Service 解锁 / 用户确认的上限。
#[cfg(not(test))]
const KEYRING_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(not(test))]
static CACHED_KEY: Mutex<Option<Zeroizing<[u8; 32]>>> = Mutex::new(None);

/// 加密一段敏感信息。
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    let key = vault_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|err| {
        ZsshError::Secret(
            Reason::new(rk::VAULT_CRYPTO_FAILED, format!("初始化资料库密钥失败：{err}"))
                .with("cause", err),
        )
    })?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill(&mut nonce);
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: MAGIC,
            },
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(rk::VAULT_CRYPTO_FAILED, format!("加密失败：{err}"))
                    .with("cause", err),
            )
        })?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// 解密 [`protect`] 写出来的密文。
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    if blob.len() < MAGIC.len() + NONCE_LEN + TAG_LEN || &blob[..MAGIC.len()] != MAGIC {
        return Err(ZsshError::Secret(Reason::new(
            rk::VAULT_CORRUPT,
            "不是 ZeeTerm Linux 资料库密文（格式不识别）",
        )));
    }
    let key = vault_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|err| {
        ZsshError::Secret(
            Reason::new(rk::VAULT_CRYPTO_FAILED, format!("初始化资料库密钥失败：{err}"))
                .with("cause", err),
        )
    })?;
    let nonce = XNonce::from_slice(&blob[MAGIC.len()..MAGIC.len() + NONCE_LEN]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &blob[MAGIC.len() + NONCE_LEN..],
                aad: MAGIC,
            },
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(rk::VAULT_CORRUPT, format!("资料库密文解不开：{err}"))
                    .with("cause", err),
            )
        })
}

/// 取资料库密钥；第一次调用会访问 Secret Service，之后缓存到进程结束。
fn vault_key() -> Result<Zeroizing<[u8; 32]>, ZsshError> {
    // 单元测试用固定密钥，只验证 AEAD 本身；生产构建没有这条路径。
    #[cfg(test)]
    {
        Ok(Zeroizing::new([0x5a; 32]))
    }

    #[cfg(not(test))]
    {
        if let Some(key) = CACHED_KEY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return Ok(Zeroizing::new(**key));
        }
        let key = load_or_create_key()?;
        *CACHED_KEY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Zeroizing::new(*key));
        Ok(Zeroizing::new(*key))
    }
}

/// 在独立线程里访问 Secret Service：keyring 的 Entry 是同步阻塞 API，
/// 不能放在 GPUI 主线程或 Tokio worker 上等 D-Bus。
#[cfg(not(test))]
fn load_or_create_key() -> Result<Zeroizing<[u8; 32]>, ZsshError> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("zeeterm-vault-key".to_owned())
        .spawn(move || {
            let _ = tx.send(load_or_create_key_blocking());
        })
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(rk::VAULT_UNAVAILABLE, format!("启动密钥线程失败：{err}"))
                    .with("cause", err),
            )
        })?;
    match rx.recv_timeout(KEYRING_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(ZsshError::Secret(Reason::new(
            rk::VAULT_UNAVAILABLE,
            "等待系统钥匙串超时（可能被锁住或需要授权）",
        ))),
    }
}

#[cfg(not(test))]
fn load_or_create_key_blocking() -> Result<Zeroizing<[u8; 32]>, ZsshError> {
    use keyring::Error as KeyringError;

    let entry = keyring::Entry::new(SERVICE, ENTRY).map_err(|err| {
        ZsshError::Secret(
            Reason::new(rk::VAULT_UNAVAILABLE, format!("打开系统钥匙串失败：{err}"))
                .with("cause", err),
        )
    })?;
    match entry.get_password() {
        Ok(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|err| {
                    ZsshError::Secret(
                        Reason::new(rk::VAULT_CORRUPT, format!("资料库密钥编码损坏：{err}"))
                            .with("cause", err),
                    )
                })?;
            let key: [u8; 32] = bytes.try_into().map_err(|_| {
                ZsshError::Secret(Reason::new(
                    rk::VAULT_CORRUPT,
                    "系统钥匙串里的资料库密钥长度不对",
                ))
            })?;
            Ok(Zeroizing::new(key))
        }
        Err(KeyringError::NoEntry) => {
            let mut key = [0u8; 32];
            rand::rng().fill(&mut key);
            let encoded = base64::engine::general_purpose::STANDARD.encode(key);
            entry.set_password(&encoded).map_err(|err| {
                ZsshError::Secret(
                    Reason::new(rk::VAULT_UNAVAILABLE, format!("保存资料库密钥失败：{err}"))
                        .with("cause", err),
                )
            })?;
            Ok(Zeroizing::new(key))
        }
        Err(err) => Err(ZsshError::Secret(
            Reason::new(
                rk::VAULT_UNAVAILABLE,
                format!("读取系统钥匙串失败：{err}"),
            )
            .with("cause", err),
        )),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_restores_plaintext() {
        let plaintext = "一段带中文的密码 p@ssw0rd".as_bytes();
        let blob = protect(plaintext).expect("加密应该成功");
        assert_ne!(blob, plaintext);
        assert_eq!(unprotect(&blob).expect("解密应该成功"), plaintext);
    }

    #[test]
    fn tampering_and_foreign_formats_are_refused() {
        let mut blob = protect(b"secret").expect("加密");
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(unprotect(&blob).is_err(), "改动密文必须解不开");
        assert!(unprotect(b"not-a-vault-blob").is_err(), "旧格式/垃圾要明确拒绝");
    }

    #[test]
    fn two_encryptions_differ_because_of_the_nonce() {
        let first = protect(b"same").expect("加密");
        let second = protect(b"same").expect("加密");
        assert_ne!(first, second, "同一明文两次加密不应相同");
    }
}
