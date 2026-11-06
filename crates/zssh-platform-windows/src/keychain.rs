//! macOS 钥匙串里的资料库密钥。
//!
//! 这里只存一把 32 字节的随机密钥，按「服务名 + 账户名」定位。秘密本身不进钥匙串：
//! 调用方拿到密钥后用它做认证加密，密文仍放在原来的数据库字段里。
//!
//! 读不到时按原因分开返回，不在这里生成新密钥。已有密文对应的密钥缺失、用户取消授权、
//! 钥匙串锁定，都必须由调用方保持原样，不能换成一把新密钥再写回去。

use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, CFData, CFMutableDictionary, CFRetained, CFString, CFType,
};
use objc2_security::{
    errSecAuthFailed, errSecDuplicateItem, errSecInteractionNotAllowed, errSecInternalComponent,
    errSecItemNotFound, errSecSuccess, errSecUserCanceled, kSecAttrAccessible,
    kSecAttrAccessibleAfterFirstUnlock, kSecAttrAccount, kSecAttrService, kSecClass,
    kSecClassGenericPassword, kSecReturnData, kSecUseDataProtectionKeychain, kSecValueData,
    SecItemAdd, SecItemCopyMatching, SecItemDelete,
};

/// 钥匙串操作的结果。缺失和失败是不同的：缺失表示这台机器还没有这把密钥，
/// 失败表示系统拒绝了这次访问。
#[derive(Debug)]
pub enum KeychainError {
    /// 这个服务名和账户名下没有条目。
    Missing,
    /// 用户在系统授权框里取消了。
    Cancelled,
    /// 钥匙串锁定，或当前状态不允许交互。
    Locked,
    /// 其它系统错误，带状态码。
    Failed { status: i32 },
    /// 这个服务名和账户名下已经有条目。不覆盖：调用方要回读确认后再决定。
    Exists,
}

/// 读一条通用密码。账户名用来区分不同资料库，不从路径或用户名派生。
pub fn load(service: &str, account: &str) -> Result<Vec<u8>, KeychainError> {
    let query = base(service, account);
    let flag = cf_bool(true)?;
    query.set(unsafe { kSecReturnData }, flag.as_ref());
    let mut result: *const CFType = std::ptr::null();
    // SAFETY: query 在调用期间有效；result 由系统分配，只在成功时接管。
    let status = unsafe { SecItemCopyMatching(query.as_opaque(), &mut result) };
    if status == errSecItemNotFound {
        return Err(KeychainError::Missing);
    }
    map_status(status)?;
    if result.is_null() {
        return Err(KeychainError::Failed { status });
    }
    // SAFETY: 非空时系统返回一个 +1 的 CFData。
    let data = unsafe {
        CFRetained::<CFData>::from_raw(std::ptr::NonNull::new_unchecked(result.cast_mut().cast()))
    };
    Ok(data.to_vec())
}

/// 写入一条通用密码。已存在时返回 [`KeychainError::Exists`] 而不是覆盖：覆盖一把正在使用的
/// 资料库密钥会让已有密文全部解不开，调用方要先回读确认再决定。
///
/// 条目放进数据保护钥匙串，解锁策略是“首次解锁后即可读取”：登录后的整个会话里反复解密
/// 不会每次都弹授权框，锁屏之后才需要重新解锁。不设生物识别约束，否则每次读密钥都要验证。
pub fn store(service: &str, account: &str, secret: &[u8]) -> Result<(), KeychainError> {
    let query = base(service, account);
    let data = CFData::from_bytes(secret);
    query.set(unsafe { kSecValueData }, data.as_ref());
    let flag = cf_bool(true)?;
    query.set(unsafe { kSecUseDataProtectionKeychain }, flag.as_ref());
    let accessible = cf_static(unsafe { kSecAttrAccessibleAfterFirstUnlock });
    query.set(unsafe { kSecAttrAccessible }, accessible.as_ref());
    // SAFETY: query 在调用期间有效；不需要返回新建条目。
    let status = unsafe { SecItemAdd(query.as_opaque(), std::ptr::null_mut()) };
    map_status(status)
}

/// 删除一条通用密码。只给测试清理用，不在正常保存路径里调用。
pub fn delete(service: &str, account: &str) -> Result<(), KeychainError> {
    let query = base(service, account);
    // SAFETY: query 在调用期间有效。
    let status = unsafe { SecItemDelete(query.as_opaque()) };
    if status == errSecItemNotFound {
        return Err(KeychainError::Missing);
    }
    map_status(status)
}

fn base(service: &str, account: &str) -> CFRetained<CFMutableDictionary<CFString, CFType>> {
    let query = CFMutableDictionary::with_capacity(6);
    let service = CFString::from_str(service);
    let account = CFString::from_str(account);
    let class = cf_static(unsafe { kSecClassGenericPassword });
    let service_key = cf_static(service.as_ref());
    let account_key = cf_static(account.as_ref());
    query.set(unsafe { kSecClass }, class.as_ref());
    query.set(unsafe { kSecAttrService }, service_key.as_ref());
    query.set(unsafe { kSecAttrAccount }, account_key.as_ref());
    // 写入走数据保护钥匙串，读取和删除必须指明同一个，否则默认搜索找不到刚写入的条目。
    if let Ok(flag) = cf_bool(true) {
        query.set(unsafe { kSecUseDataProtectionKeychain }, flag.as_ref());
    }
    query
}

fn cf_bool(value: bool) -> Result<CFRetained<CFType>, KeychainError> {
    let flag = if value {
        unsafe { kCFBooleanTrue }
    } else {
        unsafe { kCFBooleanFalse }
    };
    let flag = flag.ok_or(KeychainError::Failed {
        status: errSecInternalComponent,
    })?;
    Ok(cf_static(flag))
}

/// 把一个 CF 对象转成字典值用的 `CFType`。
fn cf_static(value: &objc2_core_foundation::CFType) -> CFRetained<CFType> {
    // SAFETY: value 在 retain 返回前有效；返回值持有自己的引用。
    unsafe { CFRetained::retain(std::ptr::NonNull::from(value)) }
}

fn map_status(status: i32) -> Result<(), KeychainError> {
    if status == errSecSuccess {
        Ok(())
    } else if status == errSecDuplicateItem {
        Err(KeychainError::Exists)
    } else if status == errSecUserCanceled {
        Err(KeychainError::Cancelled)
    } else if status == errSecInteractionNotAllowed || status == errSecAuthFailed {
        Err(KeychainError::Locked)
    } else if status == errSecItemNotFound {
        Err(KeychainError::Missing)
    } else {
        Err(KeychainError::Failed { status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "访问真实钥匙串；仅人工明确选择专用测试条目时运行"]
    fn a_secret_roundtrips_through_the_real_keychain_and_is_removed() {
        let account = format!("zssh-vault-test-{}", std::process::id());
        let service = "ZeeTerm.vault.test";
        let _ = delete(service, &account);
        let secret = b"vault-key-roundtrip-32-bytes!!!!!";
        store(service, &account, secret).expect("钥匙串应能写入测试密钥");
        let loaded = load(service, &account).expect("钥匙串应能读回测试密钥");
        assert_eq!(loaded, secret);
        delete(service, &account).expect("测试密钥应能删除");
        assert!(matches!(
            load(service, &account),
            Err(KeychainError::Missing)
        ));
    }
}
