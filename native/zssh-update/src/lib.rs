//! 原生更新：读取 `latest.json`、选择平台包、验证 minisign 签名和签名里的版本。
//!
//! 语义沿用 Tauri updater 2.14.0（Z-SSH 的更新器）的约定：
//! - manifest 的 `version` 只是"公告版本"，本身没有被签名；
//! - 签名 trusted comment 里的 `version:` 才是包真正对应的版本；
//! - 两者不一致、缺版本、错公钥、包被改过都必须拒绝；
//! - 平台 key 保留 `windows-x86_64` 与 `windows-x86_64-nsis`；
//! - Linux 的包用 `linux-x86_64-deb` 与 `linux-x86_64-tar` 两个 key 放在同一份清单里。Windows 端的
//!   [`Manifest::select`] 只认上面两个 Windows key，清单里多出来的平台条目不影响它（有测试钉住）。
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

/// 更新配置（`updater.json`，只有 `pubkey` 和 `endpoints`），编译期嵌入。
///
/// `pubkey` 是 ZeeTerm 专用的更新签名公钥（和 Z-SSH 的分开）：由 `scripts/updater-keygen.ps1`
/// 生成，发版人把打印出来的公钥填进这个文件。还没填的时候它是占位值（见
/// [`is_placeholder_pubkey`]），发行构建据此拒绝安装任何更新。
/// 更新源指向 ZeeTerm 自己的发布仓库，**绝不能指回 Z-SSH 的**：那份清单里是 Tauri 版的安装包，
/// 拉下来就把 ZeeTerm 装回成 Tauri 版。
const UPDATER_CONF: &str = include_str!("../updater.json");

/// `updater.json` 里 `pubkey` 还没填真实公钥时的占位值前缀。
pub const PUBKEY_PLACEHOLDER: &str = "PLACEHOLDER";

/// 公钥是不是还没配置的占位值。占位时发行构建不检查更新、不下载、不安装：没有公钥就没法证明
/// 安装包来自发布者，宁可不更新也不能放行。
pub fn is_placeholder_pubkey(pubkey: &str) -> bool {
    pubkey
        .trim()
        .to_ascii_uppercase()
        .starts_with(PUBKEY_PLACEHOLDER)
}

/// 编译期嵌入的更新器配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdaterConfig {
    /// base64 的 minisign 公钥。
    pub pubkey: String,
    /// 更新清单地址，按顺序尝试。
    pub endpoints: Vec<String>,
}

impl UpdaterConfig {
    /// 更新签名公钥是否已经配置（不是占位值）。
    pub fn signing_configured(&self) -> bool {
        !is_placeholder_pubkey(&self.pubkey)
    }
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

/// 由更新源地址推出发布页（`https://github.com/<owner>/<repo>/releases`）。自动更新走不通时（比如网络不通）
/// 用户可以"打开发布页"手动下载。只认 `https://github.com/<owner>/<repo>/releases/...`
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

/// `latest.json` 里 Linux x86_64 `.deb` 包的平台 key（用系统包管理器装的）。
pub const PLATFORM_LINUX_DEB: &str = "linux-x86_64-deb";

/// `latest.json` 里 Linux x86_64 `.tar.gz` 包的平台 key（`install.sh` 装的或手动解压的）。
pub const PLATFORM_LINUX_TAR: &str = "linux-x86_64-tar";

/// 发版时允许写进 `latest.json` 的 Linux 平台 key。
pub const LINUX_PLATFORM_KEYS: &[&str] = &[PLATFORM_LINUX_DEB, PLATFORM_LINUX_TAR];

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

/// 签名没验过的具体原因：类型 + 参数，界面按语言出词；`Display` 的中文留给日志和测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureProblem {
    /// 安装包内容与签名对不上（被改动过，或下载不完整）。
    Mismatch,
    /// 签名不是用 ZeeTerm 的更新密钥签的。
    WrongKey,
    /// 签名或公钥的格式不对。
    BadEncoding,
    /// 不支持的签名算法。
    UnsupportedAlgorithm,
    /// 安装包地址不是 https（生成清单时拒绝）。
    HttpsRequired { url: String },
    /// 安装包文件名里有控制字符（生成签名注释时拒绝）。
    ControlCharacters,
    /// 签名库给的其他原因，原样带上（英文原文）。
    Other(String),
}

impl std::fmt::Display for SignatureProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mismatch => {
                f.write_str("安装包内容与签名对不上（文件可能被改动过，或下载不完整）")
            }
            Self::WrongKey => f.write_str("签名不是用 ZeeTerm 的更新密钥签的"),
            Self::BadEncoding => f.write_str("签名或公钥的格式不对"),
            Self::UnsupportedAlgorithm => f.write_str("不支持的签名算法"),
            Self::HttpsRequired { url } => write!(f, "安装包地址必须是 https：{url}"),
            Self::ControlCharacters => f.write_str("安装包文件名里有控制字符"),
            Self::Other(text) => f.write_str(text),
        }
    }
}

#[derive(Debug)]
pub enum UpdateError {
    Json(serde_json::Error),
    BadVersion(String),
    NoPlatform,
    MissingSignature,
    Base64(String),
    Signature(SignatureProblem),
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

/// 发版时写进 `latest.json` 的平台键（只写通用的 Windows 键；解析端两个键都认）。
pub const RELEASE_PLATFORM_KEY: &str = "windows-x86_64";

/// 发版用的清单参数。
#[derive(Debug, Clone)]
pub struct ReleaseManifest<'a> {
    pub version: &'a str,
    pub notes: &'a str,
    /// RFC 3339 时间，例如 `2026-11-03T08:00:00Z`。
    pub pub_date: &'a str,
    /// 安装包的下载地址。
    pub url: &'a str,
    /// 安装包 `.minisig` 文件全文的 base64（`sign_installer` 的结果）。
    pub signature: &'a str,
}

/// 清单里 Windows 以外的一个平台包（见 [`build_latest_json_with_platforms`]）。`key` 只能是 [`LINUX_PLATFORM_KEYS`] 里的一个。
#[derive(Debug, Clone)]
pub struct PlatformAsset<'a> {
    pub key: &'a str,
    pub url: &'a str,
    /// 该包 `.minisig` 文件全文的 base64。
    pub signature: &'a str,
}

#[derive(serde::Serialize)]
struct ManifestOut<'a> {
    version: &'a str,
    notes: &'a str,
    pub_date: &'a str,
    platforms: BTreeMap<&'a str, AssetOut<'a>>,
}

#[derive(serde::Serialize)]
struct AssetOut<'a> {
    signature: &'a str,
    url: &'a str,
}

/// 生成 `latest.json` 文本。版本必须是合法 semver（不带 `v`）、地址必须是 https、签名不能为空；
/// 生成的文本能被 [`parse_manifest`] 读回来，这是单测钉住的往返。
pub fn build_latest_json(release: &ReleaseManifest<'_>) -> Result<String, UpdateError> {
    build_latest_json_with_platforms(release, &[])
}

/// 和 [`build_latest_json`] 一样，另外把 Windows 以外的平台包（Linux 的 deb / tar.gz）写进 `platforms`。
/// `extra` 里的 key 必须是 [`LINUX_PLATFORM_KEYS`] 之一且不能重复，地址必须是 https、签名不能为空；
/// Windows 的那一项原样保留，所以老版本的 Windows 客户端读到这份清单选中的包和以前完全一样。
pub fn build_latest_json_with_platforms(
    release: &ReleaseManifest<'_>,
    extra: &[PlatformAsset<'_>],
) -> Result<String, UpdateError> {
    if release.version.starts_with('v') || Version::parse(release.version).is_err() {
        return Err(UpdateError::BadVersion(release.version.to_owned()));
    }
    if release.signature.trim().is_empty() {
        return Err(UpdateError::MissingSignature);
    }
    if !release.url.starts_with("https://") {
        return Err(UpdateError::Signature(SignatureProblem::HttpsRequired {
            url: release.url.to_owned(),
        }));
    }
    let mut platforms = BTreeMap::new();
    platforms.insert(
        RELEASE_PLATFORM_KEY,
        AssetOut {
            signature: release.signature.trim(),
            url: release.url,
        },
    );
    for extra in extra {
        // 只收认识的 Linux key：写错 key 的清单发出去，对应平台永远选不到包，所以在这里就拒绝。
        if !LINUX_PLATFORM_KEYS.contains(&extra.key) {
            return Err(UpdateError::Signature(SignatureProblem::Other(format!(
                "不认识的平台 key：{}",
                extra.key
            ))));
        }
        if extra.signature.trim().is_empty() {
            return Err(UpdateError::MissingSignature);
        }
        if !extra.url.starts_with("https://") {
            return Err(UpdateError::Signature(SignatureProblem::HttpsRequired {
                url: extra.url.to_owned(),
            }));
        }
        let duplicate = platforms
            .insert(
                extra.key,
                AssetOut {
                    signature: extra.signature.trim(),
                    url: extra.url,
                },
            )
            .is_some();
        if duplicate {
            return Err(UpdateError::Signature(SignatureProblem::Other(format!(
                "平台 key 重复：{}",
                extra.key
            ))));
        }
    }
    let out = ManifestOut {
        version: release.version,
        notes: release.notes,
        pub_date: release.pub_date,
        platforms,
    };
    serde_json::to_string_pretty(&out).map_err(UpdateError::Json)
}

/// 签名的 trusted comment：`version:` 是验签时和清单公告版本对账的那一项（见
/// [`verify_signature`]），`file:` 记安装包文件名，`timestamp:` 是签名时间（Unix 秒）。
/// 字段用制表符隔开，所以文件名里不能有制表符和换行。
pub fn trusted_comment(
    version: &str,
    file_name: &str,
    timestamp: u64,
) -> Result<String, UpdateError> {
    if Version::parse(version).is_err() {
        return Err(UpdateError::BadVersion(version.to_owned()));
    }
    if file_name.chars().any(|c| c.is_control()) {
        return Err(UpdateError::Signature(SignatureProblem::ControlCharacters));
    }
    Ok(format!(
        "timestamp:{timestamp}\tfile:{file_name}\tversion:{version}"
    ))
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
        PublicKey::decode(&pubkey_text).map_err(|e| UpdateError::Signature(describe(&e)))?;

    let signature_text = decode_text(signature_b64)?;
    let signature =
        Signature::decode(&signature_text).map_err(|e| UpdateError::Signature(describe(&e)))?;

    // `true` 与 Tauri 2.14.0 一致：兼容旧版 minisign 生成的签名。
    public_key
        .verify(data, &signature, true)
        .map_err(|e| UpdateError::Signature(describe(&e)))?;

    // 全局签名通过之后才能信任 trusted comment。
    verify_signed_version(
        signature.trusted_comment(),
        announced_version,
        require_signed_version,
    )
}

/// minisign-verify 的错误原文是英文；这里归成几类（拿不准的原样带上）。
fn describe(err: &minisign_verify::Error) -> SignatureProblem {
    use minisign_verify::Error;
    match err {
        Error::InvalidSignature => SignatureProblem::Mismatch,
        Error::UnexpectedKeyId => SignatureProblem::WrongKey,
        Error::InvalidEncoding => SignatureProblem::BadEncoding,
        Error::UnexpectedAlgorithm | Error::UnsupportedAlgorithm => {
            SignatureProblem::UnsupportedAlgorithm
        }
        other => SignatureProblem::Other(other.to_string()),
    }
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
        // 内嵌配置的第一个源应该能推出发布页（关于页"打开发布页"按钮靠它）。
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
        // 关于页「打开发布页」由它推出来。
        assert_eq!(
            releases_page_url(&config.endpoints[0]).as_deref(),
            Some("https://github.com/MoeShinX/ZeeTerm/releases")
        );
    }

    #[test]
    fn embedded_public_key_is_a_real_key_or_the_documented_placeholder() {
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        if config.signing_configured() {
            let text = decode_text(&config.pubkey).expect("公钥 base64");
            PublicKey::decode(&text).expect("配置了的公钥应能解析");
        } else {
            // 占位值：发行构建据此拒绝安装任何更新（update_demo 里有对应测试）。
            assert!(is_placeholder_pubkey(&config.pubkey));
            assert!(
                decode_text(&config.pubkey).is_err()
                    || PublicKey::decode(&decode_text(&config.pubkey).unwrap_or_default()).is_err(),
                "占位值不能恰好是一把能用的公钥"
            );
        }
    }

    #[test]
    fn placeholder_detection_ignores_case_and_spaces_but_not_real_keys() {
        assert!(is_placeholder_pubkey("PLACEHOLDER"));
        assert!(is_placeholder_pubkey(
            "  placeholder-run-the-keygen-script "
        ));
        let (_kp, pubkey) = keypair();
        assert!(!is_placeholder_pubkey(&pubkey));
        let configured = UpdaterConfig {
            pubkey,
            endpoints: vec!["https://example.com/latest.json".to_owned()],
        };
        assert!(configured.signing_configured());
        let unconfigured = UpdaterConfig {
            pubkey: "PLACEHOLDER".to_owned(),
            ..configured
        };
        assert!(!unconfigured.signing_configured());
    }

    #[test]
    fn generated_latest_json_round_trips_through_the_parser_and_the_verifier() {
        let (kp, pubkey) = keypair();
        let installer = b"installer bytes for the round trip";
        let comment = trusted_comment("1.4.0", "ZeeTerm-Setup-1.4.0.exe", 1_700_000_000).unwrap();
        let signature = sign(&kp, installer, &comment);
        let json = build_latest_json(&ReleaseManifest {
            version: "1.4.0",
            notes: "修复若干问题\n- 第二行",
            pub_date: "2026-11-03T08:00:00Z",
            url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-Setup-1.4.0.exe",
            signature: &signature,
        })
        .expect("生成清单");

        let manifest = parse_manifest(&json).expect("清单能读回来");
        assert_eq!(manifest.version, "1.4.0");
        assert_eq!(manifest.notes.as_deref(), Some("修复若干问题\n- 第二行"));
        assert_eq!(manifest.date.as_deref(), Some("2026-11-03T08:00:00Z"));
        let package = manifest.select("1.3.9").unwrap().expect("比 1.3.9 新");
        assert_eq!(package.platform_key, RELEASE_PLATFORM_KEY);
        assert_eq!(package.version, "1.4.0");
        assert!(package.url.ends_with("/v1.4.0/ZeeTerm-Setup-1.4.0.exe"));
        verify_signature(
            installer,
            &package.signature,
            &pubkey,
            &package.version,
            true,
        )
        .expect("生成的签名能通过验签和版本绑定");
        assert!(manifest.select("1.4.0").unwrap().is_none(), "同版本不更新");
    }

    #[test]
    fn latest_json_generation_refuses_bad_input() {
        let ok = ReleaseManifest {
            version: "1.0.0",
            notes: "",
            pub_date: "2026-11-03T08:00:00Z",
            url: "https://example.com/a.exe",
            signature: "c2ln",
        };
        assert!(build_latest_json(&ok).is_ok());
        for bad_version in ["v1.0.0", "1.0", "latest", ""] {
            let release = ReleaseManifest {
                version: bad_version,
                ..ok.clone()
            };
            assert!(
                matches!(build_latest_json(&release), Err(UpdateError::BadVersion(_))),
                "{bad_version}"
            );
        }
        let release = ReleaseManifest {
            signature: "  ",
            ..ok.clone()
        };
        assert!(matches!(
            build_latest_json(&release),
            Err(UpdateError::MissingSignature)
        ));
        let release = ReleaseManifest {
            url: "http://example.com/a.exe",
            ..ok
        };
        assert!(build_latest_json(&release).is_err(), "清单里不放 http 地址");
    }

    fn linux_release<'a>(deb_signature: &'a str, tar_signature: &'a str) -> [PlatformAsset<'a>; 2] {
        [
            PlatformAsset {
                key: PLATFORM_LINUX_DEB,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.deb",
                signature: deb_signature,
            },
            PlatformAsset {
                key: PLATFORM_LINUX_TAR,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                signature: tar_signature,
            },
        ]
    }

    fn windows_release(signature: &str) -> ReleaseManifest<'_> {
        ReleaseManifest {
            version: "1.4.0",
            notes: "notes",
            pub_date: "2026-11-03T08:00:00Z",
            url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-Setup-1.4.0.exe",
            signature,
        }
    }

    /// 清单里加了 Linux 条目之后，Windows 客户端读到的还是原来的包（老版本客户端也是这套
    /// `parse_manifest` + `select`，只认 Windows 的两个 key）。
    #[test]
    fn linux_entries_leave_the_windows_selection_unchanged() {
        let (kp, pubkey) = keypair();
        let installer = b"windows installer";
        let deb = b"deb bytes";
        let tar = b"tar bytes";
        let win_sig = sign(
            &kp,
            installer,
            &trusted_comment("1.4.0", "ZeeTerm-Setup-1.4.0.exe", 1).unwrap(),
        );
        let deb_sig = sign(
            &kp,
            deb,
            &trusted_comment("1.4.0", "ZeeTerm-1.4.0-linux-x86_64.deb", 2).unwrap(),
        );
        let tar_sig = sign(
            &kp,
            tar,
            &trusted_comment("1.4.0", "ZeeTerm-1.4.0-linux-x86_64.tar.gz", 3).unwrap(),
        );

        let windows_only = build_latest_json(&windows_release(&win_sig)).expect("只有 Windows");
        let with_linux = build_latest_json_with_platforms(
            &windows_release(&win_sig),
            &linux_release(&deb_sig, &tar_sig),
        )
        .expect("带 Linux 条目");

        let before = parse_manifest(&windows_only)
            .unwrap()
            .select("1.3.0")
            .unwrap()
            .expect("有更新");
        let manifest = parse_manifest(&with_linux).expect("带 Linux 条目的清单能读");
        let after = manifest.select("1.3.0").unwrap().expect("有更新");
        assert_eq!(before, after, "Windows 选中的包必须和没有 Linux 条目时一样");
        assert_eq!(after.platform_key, RELEASE_PLATFORM_KEY);
        assert!(after.url.ends_with("ZeeTerm-Setup-1.4.0.exe"));
        assert!(manifest.select("1.4.0").unwrap().is_none(), "同版本不更新");

        // 清单里确实有两个 Linux 条目，签名各自能验过（文件名和版本都绑在受信注释里）。
        let deb_asset = manifest
            .platforms
            .get(PLATFORM_LINUX_DEB)
            .expect("deb 条目");
        let tar_asset = manifest
            .platforms
            .get(PLATFORM_LINUX_TAR)
            .expect("tar 条目");
        assert!(
            deb_asset
                .url
                .ends_with("/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.deb")
        );
        assert!(
            tar_asset
                .url
                .ends_with("/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.tar.gz")
        );
        verify_signature(
            deb,
            deb_asset.signature.as_deref().unwrap(),
            &pubkey,
            "1.4.0",
            true,
        )
        .expect("deb 签名");
        verify_signature(
            tar,
            tar_asset.signature.as_deref().unwrap(),
            &pubkey,
            "1.4.0",
            true,
        )
        .expect("tar 签名");
        // 用 deb 的签名去验 tar 的内容必须失败。
        assert!(
            verify_signature(
                tar,
                deb_asset.signature.as_deref().unwrap(),
                &pubkey,
                "1.4.0",
                true
            )
            .is_err()
        );
    }

    /// 手写的、同时带 nsis / 通用 Windows 键和 Linux 键的清单：优先级仍是 nsis。
    #[test]
    fn linux_keys_never_win_over_the_windows_keys() {
        let json = r#"{
            "version": "2.0.0",
            "platforms": {
                "linux-x86_64-deb": { "url": "https://example.com/a.deb", "signature": "c2ln" },
                "linux-x86_64-tar": { "url": "https://example.com/a.tar.gz", "signature": "c2ln" },
                "windows-x86_64": { "url": "https://example.com/a.exe", "signature": "c2ln" },
                "windows-x86_64-nsis": { "url": "https://example.com/a-setup.exe", "signature": "c2ln" }
            }
        }"#;
        let package = parse_manifest(json)
            .unwrap()
            .select("1.0.0")
            .unwrap()
            .unwrap();
        assert_eq!(package.platform_key, "windows-x86_64-nsis");
        assert_eq!(package.url, "https://example.com/a-setup.exe");
        assert!(
            PLATFORM_KEYS
                .iter()
                .all(|key| !LINUX_PLATFORM_KEYS.contains(key))
        );
        assert_eq!(
            LINUX_PLATFORM_KEYS,
            &["linux-x86_64-deb", "linux-x86_64-tar"]
        );
    }

    /// 只有 Linux 条目的清单（不会由发版流程产生）在 Windows 上就是"没有适用的包"，不会误选 Linux 包。
    #[test]
    fn a_linux_only_manifest_gives_windows_no_package() {
        let json = r#"{
            "version": "2.0.0",
            "platforms": {
                "linux-x86_64-deb": { "url": "https://example.com/a.deb", "signature": "c2ln" }
            }
        }"#;
        let manifest = parse_manifest(json).unwrap();
        assert!(matches!(
            manifest.select("1.0.0"),
            Err(UpdateError::NoPlatform)
        ));
        assert!(manifest.select("2.0.0").unwrap().is_none());
    }

    #[test]
    fn linux_platform_generation_refuses_bad_input() {
        let release = windows_release("c2ln");
        let ok = linux_release("c2ln", "c2ln");
        assert!(build_latest_json_with_platforms(&release, &ok).is_ok());
        assert!(build_latest_json_with_platforms(&release, &[]).is_ok());

        // 不认识的 key（含 Windows 的 key：Windows 那一项只能由 `release` 自己给）。
        for key in [
            "linux-x86_64",
            "linux-aarch64-deb",
            "windows-x86_64",
            "darwin-x86_64",
        ] {
            let bad = [PlatformAsset {
                key,
                ..ok[0].clone()
            }];
            assert!(
                build_latest_json_with_platforms(&release, &bad).is_err(),
                "{key}"
            );
        }
        // 同一个 key 写两次。
        let twice = [ok[0].clone(), ok[0].clone()];
        assert!(build_latest_json_with_platforms(&release, &twice).is_err());
        // 空签名、http 地址。
        let unsigned = [PlatformAsset {
            signature: "  ",
            ..ok[0].clone()
        }];
        assert!(matches!(
            build_latest_json_with_platforms(&release, &unsigned),
            Err(UpdateError::MissingSignature)
        ));
        let insecure = [PlatformAsset {
            url: "http://example.com/a.deb",
            ..ok[0].clone()
        }];
        assert!(build_latest_json_with_platforms(&release, &insecure).is_err());
    }

    #[test]
    fn trusted_comment_binds_the_version_and_rejects_odd_input() {
        let comment = trusted_comment("1.2.3", "ZeeTerm-Setup-1.2.3.exe", 5).unwrap();
        assert_eq!(
            comment,
            "timestamp:5\tfile:ZeeTerm-Setup-1.2.3.exe\tversion:1.2.3"
        );
        assert_eq!(signed_version(&comment), Some("1.2.3"));
        assert!(trusted_comment("1.2", "a.exe", 0).is_err());
        assert!(trusted_comment("1.2.3", "a\tb.exe", 0).is_err());
        assert!(trusted_comment("1.2.3", "a\nb.exe", 0).is_err());
    }

    /// 发行版只许 https：配置里混进 http 更新源就是事故。
    #[test]
    fn embedded_config_only_has_https_endpoints() {
        let config = embedded_config().expect("内嵌的更新配置应能解析");
        assert!(
            config
                .endpoints
                .iter()
                .all(|url| url.starts_with("https://")),
            "{:?}",
            config.endpoints
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
        assert!(err.to_string().contains("对不上"), "{err}");
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
        assert!(err.to_string().contains("更新密钥"), "{err}");
    }
}
