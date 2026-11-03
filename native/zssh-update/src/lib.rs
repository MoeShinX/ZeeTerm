//! 原生更新：读取 `latest.json`、选择平台包、验证 minisign 签名和签名里的版本。
//!
//! 语义与已安装的 Tauri updater 2.14.0 保持一致：
//! - manifest 的 `version` 只是"公告版本"，本身没有被签名；
//! - 签名 trusted comment 里的 `version:` 才是包真正对应的版本；
//! - 两者不一致、缺版本、错公钥、包被改过都必须拒绝；
//! - 平台 key 保留 `windows-x86_64` 与 `windows-x86_64-nsis`。
//!
//! 本 crate 不负责下载、不启动安装器，也不依赖 Tauri/GPUI。

use std::collections::BTreeMap;

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

pub mod install;

pub use install::{
    InstallError, InstallPlan, InstallRequest, command_line, plan_install, sha256_hex,
    verify_plan_file,
};

/// 更新配置（`updater.json`，只有 `pubkey` 和 `endpoints`），编译期嵌入。公钥暂时沿用 Tauri 版的那一把
/// （发版签名没换）；更新源指向 ZeeTerm 自己的发布仓库，**绝不能指回 Z-SSH 的**：那份清单里是 Tauri 版的安装包，
/// 拉下来就把 ZeeTerm 装回成 Tauri 版。
/// 这份文件放在本 crate 里而不是读 `src-tauri/tauri.conf.json`，是为了让原生版能脱离 Tauri 版
/// 单独成库；在同时带着 Tauri 版的仓库里，有测试保证两份内容一致。
const UPDATER_CONF: &str = include_str!("../updater.json");

/// 编译期嵌入的更新器配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdaterConfig {
    /// base64 的 minisign 公钥。
    pub pubkey: String,
    /// 更新清单地址，按顺序尝试。
    pub endpoints: Vec<String>,
}

/// 取内嵌的更新器配置；配置文件被改坏时返回原因（有单测保证仓库里的这份能解析）。
pub fn embedded_config() -> Result<UpdaterConfig, String> {
    parse_updater_config(UPDATER_CONF)
}

fn parse_updater_config(json: &str) -> Result<UpdaterConfig, String> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|err| format!("更新配置不是合法 JSON：{err}"))?;
    let updater = &root;
    let pubkey = updater["pubkey"]
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "更新配置里没有公钥".to_owned())?
        .to_owned();
    let endpoints: Vec<String> = updater["endpoints"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|value| value.as_str())
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if endpoints.is_empty() {
        return Err("更新配置里没有更新源".to_owned());
    }
    Ok(UpdaterConfig { pubkey, endpoints })
}

/// 由更新源地址推出发布页（`https://github.com/<owner>/<repo>/releases`）。试用版不能自动更新时，
/// "打开发布页"打开它让用户手动下载。只认 `https://github.com/<owner>/<repo>/releases/...`
/// 形式的地址，owner / repo 只允许常见的名字字符；推不出来返回 `None`（界面就不显示按钮）。
pub fn releases_page_url(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("https://github.com/")?;
    let cut = rest.find("/releases/")?;
    let mut parts = rest[..cut].split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let safe = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if parts.next().is_some() || !safe(owner) || !safe(name) {
        return None;
    }
    Some(format!("https://github.com/{owner}/{name}/releases"))
}

/// 自动更新优先用安装版包；没有时退回通用 Windows key。
pub const PLATFORM_KEYS: &[&str] = &["windows-x86_64-nsis", "windows-x86_64"];

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default, alias = "body")]
    pub notes: Option<String>,
    #[serde(default, alias = "pub_date")]
    pub date: Option<String>,
    pub platforms: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub url: String,
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub platform_key: String,
    pub version: String,
    pub url: String,
    pub signature: String,
}

#[derive(Debug)]
pub enum UpdateError {
    Json(serde_json::Error),
    BadVersion(String),
    NoPlatform,
    MissingSignature,
    Base64(String),
    Signature(String),
    MissingSignedVersion,
    SignedVersionMismatch { signed: String, announced: String },
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(err) => write!(f, "更新说明格式错误：{err}"),
            Self::BadVersion(version) => write!(f, "版本号不合法：{version}"),
            Self::NoPlatform => write!(f, "没有适用于当前平台的安装包"),
            Self::MissingSignature => write!(f, "安装包缺少签名"),
            Self::Base64(err) => write!(f, "签名编码错误：{err}"),
            Self::Signature(err) => write!(f, "签名校验未通过：{err}"),
            Self::MissingSignedVersion => write!(f, "签名里没有版本号，拒绝安装"),
            Self::SignedVersionMismatch { signed, announced } => {
                write!(f, "签名版本 {signed} 与公告版本 {announced} 不一致")
            }
        }
    }
}

impl std::error::Error for UpdateError {}

pub fn parse_manifest(json: &str) -> Result<Manifest, UpdateError> {
    serde_json::from_str(json).map_err(UpdateError::Json)
}

impl Manifest {
    /// 公告版本必须严格高于当前版本，才返回可下载的包。
    pub fn select(&self, current: &str) -> Result<Option<Package>, UpdateError> {
        let announced = Version::parse(self.version.trim_start_matches('v'))
            .map_err(|_| UpdateError::BadVersion(self.version.clone()))?;
        let current = Version::parse(current.trim_start_matches('v'))
            .map_err(|_| UpdateError::BadVersion(current.to_owned()))?;
        if announced <= current {
            return Ok(None);
        }

        for key in PLATFORM_KEYS {
            let Some(asset) = self.platforms.get(*key) else {
                continue;
            };
            let Some(signature) = asset.signature.as_deref().filter(|s| !s.trim().is_empty())
            else {
                return Err(UpdateError::MissingSignature);
            };
            return Ok(Some(Package {
                platform_key: (*key).to_owned(),
                version: announced.to_string(),
                url: asset.url.clone(),
                signature: signature.to_owned(),
            }));
        }
        Err(UpdateError::NoPlatform)
    }
}

/// 验证包字节、签名、以及 trusted comment 里的版本绑定。
pub fn verify_signature(
    data: &[u8],
    signature_b64: &str,
    pubkey_b64: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> Result<(), UpdateError> {
    let pubkey_text = decode_text(pubkey_b64)?;
    let public_key =
        PublicKey::decode(&pubkey_text).map_err(|e| UpdateError::Signature(e.to_string()))?;

    let signature_text = decode_text(signature_b64)?;
    let signature =
        Signature::decode(&signature_text).map_err(|e| UpdateError::Signature(e.to_string()))?;

    // `true` 与 Tauri 2.14.0 一致：兼容旧版 minisign 生成的签名。
    public_key
        .verify(data, &signature, true)
        .map_err(|e| UpdateError::Signature(e.to_string()))?;

    // 全局签名通过之后才能信任 trusted comment。
    verify_signed_version(
        signature.trusted_comment(),
        announced_version,
        require_signed_version,
    )
}

fn decode_text(value: &str) -> Result<String, UpdateError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|e| UpdateError::Base64(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| UpdateError::Base64(e.to_string()))
}

fn verify_signed_version(
    trusted_comment: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> Result<(), UpdateError> {
    let Some(signed) = signed_version(trusted_comment) else {
        return if require_signed_version {
            Err(UpdateError::MissingSignedVersion)
        } else {
            Ok(())
        };
    };

    let matches = match (
        Version::parse(signed.trim_start_matches('v')),
        Version::parse(announced_version.trim_start_matches('v')),
    ) {
        (Ok(signed), Ok(announced)) => signed == announced,
        _ => signed == announced_version,
    };

    if matches {
        Ok(())
    } else {
        Err(UpdateError::SignedVersionMismatch {
            signed: signed.to_owned(),
            announced: announced_version.to_owned(),
        })
    }
}

fn signed_version(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|part| part.strip_prefix("version:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    fn sign(kp: &KeyPair, data: &[u8], comment: &str) -> String {
        let signed = minisign::sign(Some(&kp.pk), &kp.sk, data, Some(comment), Some("zssh test"))
            .expect("签名");
        base64::engine::general_purpose::STANDARD.encode(signed.to_string())
    }

    fn manifest(version: &str, signature: &str) -> String {
        format!(
            r#"{{
                "version": "{version}",
                "notes": "test",
                "pub_date": "2026-10-30T00:00:00Z",
                "platforms": {{
                    "windows-x86_64": {{ "url": "https://example.com/a.exe", "signature": "{signature}" }},
                    "windows-x86_64-nsis": {{ "url": "https://example.com/a-setup.exe", "signature": "{signature}" }}
                }}
            }}"#
        )
    }

    fn keypair() -> (KeyPair, String) {
        let kp = KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
        let text = kp.pk.to_box().expect("公钥文本").to_string();
        let pubkey = base64::engine::general_purpose::STANDARD.encode(text);
        (kp, pubkey)
    }

    #[test]
    fn releases_page_is_derived_from_a_github_endpoint_only() {
        let endpoint = "https://github.com/owner-1/repo.name/releases/latest/download/latest.json";
        assert_eq!(
            releases_page_url(endpoint).as_deref(),
            Some("https://github.com/owner-1/repo.name/releases")
        );
        for bad in [
            "http://github.com/a/b/releases/x",
            "https://example.com/a/b/releases/x",
            "https://github.com/a/b/c/releases/x",
            "https://github.com/a b/c/releases/x",
            "https://github.com/a/b",
            "https://github.com//b/releases/x",
        ] {
            assert_eq!(releases_page_url(bad), None, "{bad}");
        }
        // 内嵌配置的第一个源应该能推出发布页（试用版"打开发布页"按钮靠它）。
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        assert!(releases_page_url(&config.endpoints[0]).is_some_and(|url| {
            url.starts_with("https://github.com/") && url.ends_with("/releases")
        }));
    }
    #[test]
    fn updates_come_from_the_zeeterm_repository_never_from_z_ssh() {
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        assert!(!config.endpoints.is_empty());
        for endpoint in &config.endpoints {
            assert!(
                endpoint.starts_with("https://github.com/MoeShinX/ZeeTerm/releases/"),
                "{endpoint}"
            );
            assert!(
                !endpoint.to_ascii_lowercase().contains("z-ssh"),
                "更新源不能指向 Z-SSH 的仓库：{endpoint}"
            );
        }
        // 试用版「打开发布页」由它推出来。
        assert_eq!(
            releases_page_url(&config.endpoints[0]).as_deref(),
            Some("https://github.com/MoeShinX/ZeeTerm/releases")
        );
    }

    #[test]
    fn production_public_key_decodes() {
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        let text = decode_text(&config.pubkey).expect("公钥 base64");
        PublicKey::decode(&text).expect("生产公钥应能解析");
    }

    /// 在同时带着 Tauri 版的仓库里，两份更新配置必须一致；独立导出的原生版仓库里没有
    /// `src-tauri/tauri.conf.json`，此时跳过（不是失败）。
    #[test]
    fn embedded_config_matches_tauri_conf_json_when_present() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("src-tauri")
            .join("tauri.conf.json");
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        // 发行版只许 https：配置里混进 http 更新源就是事故（不依赖 Tauri 版文件，始终检查）。
        assert!(
            config
                .endpoints
                .iter()
                .all(|url| url.starts_with("https://")),
            "{:?}",
            config.endpoints
        );
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!(
                    "跳过与 tauri.conf.json 的一致性比对：没有 src-tauri/（独立导出的原生版仓库）"
                );
                return;
            }
            Err(err) => panic!("读 {} 失败：{err}", path.display()),
        };
        let root: serde_json::Value = serde_json::from_str(&text).expect("读取 tauri.conf.json");
        let updater = &root["plugins"]["updater"];
        assert_eq!(Some(config.pubkey.as_str()), updater["pubkey"].as_str());
        let endpoints: Vec<&str> = updater["endpoints"]
            .as_array()
            .expect("配置里应有更新源")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert_eq!(
            config
                .endpoints
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            endpoints
        );
    }

    #[test]
    fn broken_updater_config_is_reported_not_guessed() {
        assert!(parse_updater_config("not json").is_err());
        assert!(parse_updater_config("{}").is_err());
        let no_endpoints = r#"{"pubkey":"abc","endpoints":[]}"#;
        assert!(parse_updater_config(no_endpoints).is_err());
        let no_key = r#"{"pubkey":" ","endpoints":["https://x/y"]}"#;
        assert!(parse_updater_config(no_key).is_err());
    }

    #[test]
    fn newer_version_selects_the_nsis_package() {
        let (_kp, pubkey) = keypair();
        let manifest = parse_manifest(&manifest("0.3.0", &pubkey)).unwrap();
        let package = manifest.select("0.2.14").unwrap().expect("应有更新");
        assert_eq!(package.platform_key, "windows-x86_64-nsis");
        assert_eq!(package.version, "0.3.0");
    }

    #[test]
    fn same_or_older_version_is_up_to_date() {
        let (_kp, pubkey) = keypair();
        for current in ["0.3.0", "0.3.1", "v0.3.0"] {
            let manifest = parse_manifest(&manifest("0.3.0", &pubkey)).unwrap();
            assert!(manifest.select(current).unwrap().is_none(), "{current}");
        }
    }

    #[test]
    fn valid_signature_with_matching_version_passes() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "timestamp:1\tversion:0.3.0");
        verify_signature(data, &sig_b64, &pubkey, "0.3.0", true).expect("签名应通过");
    }

    #[test]
    fn tampered_package_is_rejected() {
        let (kp, pubkey) = keypair();
        let sig_b64 = sign(&kp, b"installer bytes", "version:0.3.0");
        let err = verify_signature(b"tampered", &sig_b64, &pubkey, "0.3.0", true).unwrap_err();
        assert!(matches!(err, UpdateError::Signature(_)));
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "version:0.2.0");
        let err = verify_signature(data, &sig_b64, &pubkey, "0.3.0", true).unwrap_err();
        assert!(matches!(err, UpdateError::SignedVersionMismatch { .. }));
    }

    #[test]
    fn missing_signed_version_is_rejected_when_required() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "timestamp:1");
        let err = verify_signature(data, &sig_b64, &pubkey, "0.3.0", true).unwrap_err();
        assert!(matches!(err, UpdateError::MissingSignedVersion));
    }

    #[test]
    fn wrong_public_key_is_rejected() {
        let (kp, _) = keypair();
        let (_, other) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "version:0.3.0");
        let err = verify_signature(data, &sig_b64, &other, "0.3.0", true).unwrap_err();
        assert!(matches!(err, UpdateError::Signature(_)));
    }
}
