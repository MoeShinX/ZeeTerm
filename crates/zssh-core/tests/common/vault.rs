//! 集成测试只使用内存密钥，不注册或访问系统钥匙串。
#![allow(dead_code)]

#[cfg(target_os = "macos")]
fn install() {
    use std::collections::HashMap;
    use std::sync::{Mutex, Once, OnceLock};
    use zssh_core::store::secrets::{VaultBackend, VaultKeyError};
    struct Memory;
    type Items = Mutex<HashMap<(String, String), Vec<u8>>>;
    fn items() -> &'static Items {
        static ITEMS: OnceLock<Items> = OnceLock::new();
        ITEMS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    impl VaultBackend for Memory {
        fn load(&self, service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError> {
            items()
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned()
                .ok_or(VaultKeyError::Missing)
        }
        fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError> {
            let mut items = items().lock().unwrap();
            match items.entry((service.into(), account.into())) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(secret.to_vec());
                    Ok(())
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    Err(VaultKeyError::Failed { status: -1 })
                }
            }
        }
    }
    static MEMORY: Memory = Memory;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| zssh_core::store::secrets::set_vault_backend(&MEMORY));
}

#[cfg(not(target_os = "macos"))]
fn install() {}

pub fn open(
    path: &std::path::Path,
) -> Result<zssh_core::store::Database, zssh_core::error::ZsshError> {
    install();
    zssh_core::store::Database::open(path)
}

pub fn open_in_memory() -> Result<zssh_core::store::Database, zssh_core::error::ZsshError> {
    install();
    zssh_core::store::Database::open_in_memory()
}
