//! OneDrive 的连接设置和 client ID 的来源。
//!
//! client ID（Azure 里“应用程序(客户端) ID”）是公共客户端的标识，**不是秘密**，可以进仓库：
//! 程序内置一个默认的 [`DEFAULT_CLIENT_ID`]；同步设置“高级”里可以填自己的（存在本机
//! [`OneDriveConfig`] 里，随 `sync_storage` 一起 DPAPI 加密，**不同步**），填了合法值就优先用它。

use serde::{Deserialize, Serialize};

/// 一个连接上的 OneDrive 账号：非秘密的部分。令牌另存（[`super::tokens`]）。
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OneDriveConfig {
    /// 用户在“高级”里填的 client ID；空 = 用内置的那个。
    #[serde(default)]
    pub client_id: String,
    /// 登录时记下的账号显示名（界面显示用）。
    #[serde(default)]
    pub account: String,
}

impl std::fmt::Debug for OneDriveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 账号名是用户的个人信息，不进日志。
        f.debug_struct("OneDriveConfig")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// Azure 的应用程序(客户端) ID 是 GUID：`8-4-4-4-12` 个十六进制字符。
pub fn is_valid_client_id(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, part)| part.len() == *len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 内置的 client ID（公共客户端标识，不是机密）。
pub const DEFAULT_CLIENT_ID: &str = "2feedc3e-6152-4734-a68a-4a946485e1d1";

impl OneDriveConfig {
    /// 实际用的 client ID：用户在“高级”里填的合法值优先，没填或填得不合法就用内置的。
    pub fn effective_client_id(&self) -> String {
        let custom = self.client_id.trim();
        if is_valid_client_id(custom) {
            return custom.to_owned();
        }
        DEFAULT_CLIENT_ID.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn client_ids_must_look_like_azure_application_ids() {
        assert!(is_valid_client_id(ID));
        assert!(is_valid_client_id("ABCDEF01-2345-6789-abcd-ef0123456789"));
        for bad in [
            "",
            "not-a-guid",
            "11111111222233334444555555555555",
            "11111111-2222-3333-4444-55555555555",
            "11111111-2222-3333-4444-5555555555555",
            "1111111g-2222-3333-4444-555555555555",
            " 11111111-2222-3333-4444-555555555555",
            "11111111-2222-3333-4444-555555555555\n",
        ] {
            assert!(!is_valid_client_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_built_in_id_is_a_well_formed_guid() {
        assert!(is_valid_client_id(DEFAULT_CLIENT_ID));
    }

    #[test]
    fn a_custom_client_id_is_preferred_and_an_invalid_one_falls_back_to_the_built_in() {
        let custom = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let config = OneDriveConfig {
            client_id: format!("  {custom} "),
            account: String::new(),
        };
        assert_eq!(config.effective_client_id(), custom);
        for unset in ["", "   ", "oops"] {
            let config = OneDriveConfig {
                client_id: unset.to_owned(),
                account: String::new(),
            };
            assert_eq!(config.effective_client_id(), DEFAULT_CLIENT_ID, "{unset:?}");
        }
    }

    #[test]
    fn debug_output_has_no_account_name() {
        let config = OneDriveConfig {
            client_id: ID.to_owned(),
            account: "Some Person".to_owned(),
        };
        let text = format!("{config:?}");
        assert!(!text.contains("Some Person"), "{text}");
    }

    #[test]
    fn old_saved_settings_without_new_fields_still_load() {
        let config: OneDriveConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, OneDriveConfig::default());
    }
}
