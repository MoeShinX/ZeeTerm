//! macOS 资料库密钥的钥匙串后端。
//!
//! core 只定义存取接口，真正的钥匙串调用在平台 crate 里。这里做一次转接，
//! 让 `secrets::protect` / `unprotect` 在 macOS 上用得到钥匙串。

use zssh_core::store::secrets::{VaultBackend, VaultKeyError};
use zssh_platform_windows::keychain::{self, KeychainError};

pub struct KeychainVault;

pub static KEYCHAIN: KeychainVault = KeychainVault;

impl VaultBackend for KeychainVault {
    fn load(&self, service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError> {
        keychain::load(service, account).map_err(map_error)
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError> {
        keychain::store(service, account, secret).map_err(map_error)
    }
}

fn map_error(err: KeychainError) -> VaultKeyError {
    match err {
        KeychainError::Missing => VaultKeyError::Missing,
        KeychainError::Cancelled => VaultKeyError::Cancelled,
        KeychainError::Locked => VaultKeyError::Locked,
        KeychainError::Failed { status } => VaultKeyError::Failed { status },
        KeychainError::Exists => VaultKeyError::Exists,
    }
}
