//! 本机敏感信息的封装（生产用 Windows DPAPI / macOS Keychain）。
//!
//! 同步包里放的是明文（整个包由主密码加密）；写回本地库时要换成**这台电脑**
//! 的密文。每个 sealer 绑定一个资料库账户，确保多资料库的秘密隔离。测试用另一个实现模拟”另一台电脑的 DPAPI”。

use super::error::SyncError;

pub trait SecretSealer: Send + Sync {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError>;
    fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError>;
}

/// 绑定资料库的 DPAPI / Keychain sealer。
pub struct DpapiSealer {
    vault_account: Option<String>,
}

impl DpapiSealer {
    /// 创建绑定到指定资料库账户的 sealer（macOS 需要）。
    pub fn new(vault_account: String) -> Self {
        Self {
            vault_account: Some(vault_account),
        }
    }

    /// 测试用：不绑定资料库账户，使用默认密钥。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            vault_account: None,
        }
    }
}

impl SecretSealer for DpapiSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
        #[cfg(target_os = “macos”)]
        {
            if let Some(account) = &self.vault_account {
                let _scope = crate::store::secrets::VaultScope::new(account);
                crate::store::secrets::protect(plaintext).map_err(SyncError::from)
            } else {
                crate::store::secrets::protect(plaintext).map_err(SyncError::from)
            }
        }
        #[cfg(not(target_os = “macos”))]
        {
            let _ = &self.vault_account;
            crate::store::secrets::protect(plaintext).map_err(SyncError::from)
        }
    }

    fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError> {
        #[cfg(target_os = “macos”)]
        {
            if let Some(account) = &self.vault_account {
                let _scope = crate::store::secrets::VaultScope::new(account);
                crate::store::secrets::unprotect(blob).map_err(SyncError::from)
            } else {
                crate::store::secrets::unprotect(blob).map_err(SyncError::from)
            }
        }
        #[cfg(not(target_os = “macos”))]
        {
            let _ = &self.vault_account;
            crate::store::secrets::unprotect(blob).map_err(SyncError::from)
        }
    }
}

/// 测试用的“另一台电脑的 DPAPI”：带前缀的可逆混淆。
#[cfg(test)]
pub(crate) struct TestSealer(pub u8);

#[cfg(test)]
impl SecretSealer for TestSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
        let mut out = vec![b'T', self.0];
        out.extend(plaintext.iter().map(|byte| byte ^ self.0));
        Ok(out)
    }

    fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError> {
        if blob.len() < 2 || blob[0] != b'T' || blob[1] != self.0 {
            return Err(SyncError::InvalidData(crate::error::Reason::text(
                "不是 TestSealer 的密文".to_owned(),
            )));
        }
        Ok(blob[2..].iter().map(|byte| byte ^ self.0).collect())
    }
}
