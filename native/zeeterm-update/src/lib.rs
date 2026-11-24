//! 原生更新：读取 `latest.json`、选择平台包、验证 minisign 签名和签名里的版本。
//!
//! 语义沿用 Tauri updater 2.14.0（Z-SSH 的更新器）的约定：
//! - manifest 的 `version` 只是"公告版本"，本身没有被签名；
//! - 签名 trusted comment 里的 `version:` 才是包真正对应的版本；
//! - 两者不一致、缺版本、错公钥、包被改过都必须拒绝；
//! - 平台 key 保留 `windows-x86_64` 与 `windows-x86_64-nsis`；
//! - Linux 的包用 `linux-x86_64-deb`、`linux-x86_64-tar`、`linux-aarch64-deb`、`linux-aarch64-tar` 四个 key
//!   放在同一份清单里。Windows 端的
//!   [`Manifest::select`] 只认上面两个 Windows key，清单里多出来的平台条目不影响它（有测试钉住）；
//! - Windows 免安装版（zip）用 `windows-x86_64-portable` 一个 key，同样放在同一份清单里，只有免安装版自己的更新窗口会选它（见 [`portable`]）。
//!
//! 本 crate 不负责下载、不启动安装器，也不依赖 Tauri/GPUI。

use std::collections::BTreeMap;

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

pub mod install;
pub mod linux;
pub mod portable;

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

/// `latest.json` 里 Linux aarch64 `.deb` 包的平台 key。
pub const PLATFORM_LINUX_AARCH64_DEB: &str = "linux-aarch64-deb";

/// `latest.json` 里 Linux aarch64 `.tar.gz` 包的平台 key。
pub const PLATFORM_LINUX_AARCH64_TAR: &str = "linux-aarch64-tar";

/// 发版时允许写进 `latest.json` 的 Linux 平台 key（两种架构、两种安装方式）。
pub const LINUX_PLATFORM_KEYS: &[&str] = &[
    PLATFORM_LINUX_DEB,
    PLATFORM_LINUX_TAR,
    PLATFORM_LINUX_AARCH64_DEB,
    PLATFORM_LINUX_AARCH64_TAR,
];

/// `latest.json` 里 Windows 免安装版（zip）的平台 key。老客户端只认 [`PLATFORM_KEYS`] 里的两个 key，
/// 多出来的这一项对它们没有任何影响（有测试钉住）；只有免安装版自己的更新窗口会选它（见 [`portable`]）。
pub const PLATFORM_WINDOWS_PORTABLE: &str = "windows-x86_64-portable";

/// 免安装版更新选包用的 key 列表：只认自己这一种，免安装版绝不拿安装包去"更新"自己。
pub const PORTABLE_PLATFORM_KEYS: &[&str] = &[PLATFORM_WINDOWS_PORTABLE];

/// 发版时允许写进 `latest.json`、Windows 安装包之外的平台 key：Linux 的四个包加免安装版。
pub const EXTRA_PLATFORM_KEYS: &[&str] = &[
    PLATFORM_LINUX_DEB,
    PLATFORM_LINUX_TAR,
    PLATFORM_LINUX_AARCH64_DEB,
    PLATFORM_LINUX_AARCH64_TAR,
    PLATFORM_WINDOWS_PORTABLE,
];

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default, alias = "body")]
    pub notes: Option<String>,
    /// 英文更新说明（可选）：老客户端不认这个字段，照常只读 `notes`；新客户端的英文界面优先显示它。
    /// 和 `notes` 一样没有被签名——签名只绑在安装包上（见 [`verify_signature`]），清单整份不签名。
    #[serde(default)]
    pub notes_en: Option<String>,
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
    /// 下载地址里取不出安装包文件名，没法核对签名里的 `file:`。
    NoExpectedFileName,
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
            Self::NoExpectedFileName => f.write_str("下载地址里取不出安装包文件名，无法核对签名"),
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
    SignedVersionMismatch {
        signed: String,
        announced: String,
    },
    MissingSignedFile,
    /// `signed` 是签名里的文件名，`expected` 是调用方期望的文件名（应用里取自下载地址；都已去掉控制字符并截短，只用于显示）。
    SignedFileMismatch {
        signed: String,
        expected: String,
    },
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
            Self::MissingSignedFile => write!(f, "签名里没有安装包文件名，拒绝安装"),
            Self::SignedFileMismatch { signed, expected } => {
                write!(f, "签名的文件名 {signed} 与期望的文件名 {expected} 不一致")
            }
        }
    }
}

impl std::error::Error for UpdateError {}

pub fn parse_manifest(json: &str) -> Result<Manifest, UpdateError> {
    serde_json::from_str(json).map_err(UpdateError::Json)
}

impl Manifest {
    /// 公告版本必须严格高于当前版本，才返回可下载的包（Windows 的平台 key）。
    pub fn select(&self, current: &str) -> Result<Option<Package>, UpdateError> {
        self.select_for(current, PLATFORM_KEYS)
    }

    /// 和 [`Manifest::select`] 一样，只是由调用方给出本机认的平台 key（按顺序试，第一个有的算数）。
    /// Linux 按安装方式只认 deb 或只认 tar.gz（见 [`linux`] 模块），不会拿一个格式去装另一种安装。
    /// 公告版本更高、但清单里没有这些 key 的条目时返回 [`UpdateError::NoPlatform`]。
    pub fn select_for(&self, current: &str, keys: &[&str]) -> Result<Option<Package>, UpdateError> {
        let announced = Version::parse(self.version.trim_start_matches('v'))
            .map_err(|_| UpdateError::BadVersion(self.version.clone()))?;
        let current = Version::parse(current.trim_start_matches('v'))
            .map_err(|_| UpdateError::BadVersion(current.to_owned()))?;
        if announced <= current {
            return Ok(None);
        }

        for key in keys {
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
    /// 中文更新说明：老客户端只认它。
    pub notes: &'a str,
    /// 英文更新说明；`None` 时清单里没有 `notes_en` 这个字段（和以前逐字节一样）。
    pub notes_en: Option<&'a str>,
    /// RFC 3339 时间，例如 `2026-11-03T08:00:00Z`。
    pub pub_date: &'a str,
    /// 安装包的下载地址。
    pub url: &'a str,
    /// 安装包 `.minisig` 文件全文的 base64（`sign_installer` 的结果）。
    pub signature: &'a str,
}

/// 清单里 Windows 安装包以外的一个平台包（见 [`build_latest_json_with_platforms`]）。`key` 只能是 [`EXTRA_PLATFORM_KEYS`] 里的一个。
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
    #[serde(skip_serializing_if = "Option::is_none")]
    notes_en: Option<&'a str>,
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

/// 和 [`build_latest_json`] 一样，另外把 Windows 安装包以外的平台包（Linux 的 deb / tar.gz、Windows 免安装版）写进 `platforms`。
/// `extra` 里的 key 必须是 [`EXTRA_PLATFORM_KEYS`] 之一且不能重复，地址必须是 https、签名不能为空；
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
        // 只收认识的 key：写错 key 的清单发出去，对应平台永远选不到包，所以在这里就拒绝。
        if !EXTRA_PLATFORM_KEYS.contains(&extra.key) {
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
        // 空白的英文说明当没有：别让客户端拿一个空字符串盖掉中文说明。
        notes_en: release
            .notes_en
            .map(str::trim)
            .filter(|notes| !notes.is_empty()),
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

/// 验证包字节、签名、以及 trusted comment 里的版本和文件名绑定。
///
/// `latest.json` 清单本身不签名，所以"这个签名属于哪个包"只能靠签名里的 trusted comment 对账：
/// `version:` 要等于清单公告的版本，`file:` 要等于 `expected_file_name`（调用方从这个平台的下载
/// 地址取，见 [`file_name_from_url`]）。没有文件名这一项，同一版本里别的平台 / 架构的包（它们的签名
/// 同样有效）被换进清单后签名照样通过。文件名按字节完全一致比较，区分大小写。
///
/// `require_signed_version` 为 `true`（应用和发版工具都是这样）时，签名里必须同时带 `version:` 和
/// `file:`，缺哪个都拒绝。为 `false` 是为兼容更早的、没写这两项的签名保留的宽松路径：缺的那项不
/// 查，但只要签名里写了，就必须对得上。
pub fn verify_signature(
    data: &[u8],
    signature_b64: &str,
    pubkey_b64: &str,
    announced_version: &str,
    expected_file_name: &str,
    require_signed_version: bool,
) -> Result<(), UpdateError> {
    let pubkey_text = decode_text(pubkey_b64)?;
    let public_key =
        PublicKey::decode(&pubkey_text).map_err(|e| UpdateError::Signature(describe(&e)))?;

    let signature_text = decode_text(signature_b64)?;
    let signature =
        Signature::decode(&signature_text).map_err(|e| UpdateError::Signature(describe(&e)))?;

    // 第三个参数 `false`：只接受预先哈希（`ED`）的签名，不再兼容旧版 minisign 的非预哈希签名。
    // ZeeTerm 的密钥是自己生成的，发版工具（minisign crate）一直只出预哈希格式，线上已发布的包
    // 也都是；留着旧格式兼容只是多开一条没人用的路径。
    public_key
        .verify(data, &signature, false)
        .map_err(|e| UpdateError::Signature(describe(&e)))?;

    // 全局签名通过之后才能信任 trusted comment。先对版本、再对文件名。
    let comment = signature.trusted_comment();
    verify_signed_version(comment, announced_version, require_signed_version)?;
    verify_signed_file(comment, expected_file_name, require_signed_version)
}

/// 从安装包下载地址取文件名：最后一段路径，去掉查询串和片段，百分号解码，按 UTF-8 读。
///
/// 取不出合法文件名返回 `None`（调用方当验签失败）：没有路径、末尾是 `/`、整段是 `.` / `..`、
/// 解码后为空、含控制字符 / `/` / `\`、百分号转义不完整或解码后不是 UTF-8。
pub fn file_name_from_url(url: &str) -> Option<String> {
    let (_scheme, rest) = url.split_once("://")?;
    // 查询串和片段里可能带 `/`，先切掉再找路径。
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (_authority, path) = rest.split_once('/')?;
    let last = path.rsplit('/').next().unwrap_or("");
    let name = percent_decode(last)?;
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\');
    if bad { None } else { Some(name) }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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

fn signed_file(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|part| part.strip_prefix("file:"))
}

/// 核对 trusted comment 里的 `file:` 与期望的安装包文件名（按字节完全一致）。
fn verify_signed_file(
    trusted_comment: &str,
    expected_file_name: &str,
    require_signed_file: bool,
) -> Result<(), UpdateError> {
    if expected_file_name.is_empty() {
        return Err(UpdateError::Signature(SignatureProblem::NoExpectedFileName));
    }
    let Some(signed) = signed_file(trusted_comment) else {
        return if require_signed_file {
            Err(UpdateError::MissingSignedFile)
        } else {
            Ok(())
        };
    };
    if signed.as_bytes() == expected_file_name.as_bytes() {
        Ok(())
    } else {
        Err(UpdateError::SignedFileMismatch {
            signed: display_file_name(signed),
            expected: display_file_name(expected_file_name),
        })
    }
}

/// 进错误信息的文件名：去掉控制字符并截短，别让一个超长或带控制字符的名字撑坏提示。
fn display_file_name(name: &str) -> String {
    const MAX_CHARS: usize = 96;
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() > MAX_CHARS {
        let head: String = cleaned.chars().take(MAX_CHARS).collect();
        format!("{head}…")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    fn sign(kp: &KeyPair, data: &[u8], comment: &str) -> String {
        let signed = minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            data,
            Some(comment),
            Some("zeeterm test"),
        )
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
            notes_en: Some("Fixed a few problems\n- second line"),
            pub_date: "2026-11-03T08:00:00Z",
            url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-Setup-1.4.0.exe",
            signature: &signature,
        })
        .expect("生成清单");

        let manifest = parse_manifest(&json).expect("清单能读回来");
        assert_eq!(manifest.version, "1.4.0");
        assert_eq!(manifest.notes.as_deref(), Some("修复若干问题\n- 第二行"));
        assert_eq!(
            manifest.notes_en.as_deref(),
            Some("Fixed a few problems\n- second line")
        );
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
            "ZeeTerm-Setup-1.4.0.exe",
            true,
        )
        .expect("生成的签名能通过验签和版本绑定");
        assert!(manifest.select("1.4.0").unwrap().is_none(), "同版本不更新");
    }

    /// 新增的英文说明字段对老清单、老客户端都没有影响：没有它的清单照常解析（`notes_en` 为 `None`），
    /// 没传英文说明时生成的文本里根本没有这个字段（和以前逐字节一样）；老客户端的解析结构里没有这个字段，
    /// 多出来的键被忽略，读到的 `notes`、选到的包都和没有它时一样。
    #[test]
    fn the_english_notes_field_is_optional_and_does_not_disturb_older_readers() {
        // 老清单（没有 notes_en，Tauri 时代的 body / pub_date 别名也照常认）。
        let old = r#"{
            "version": "1.0.1",
            "body": "旧说明",
            "pub_date": "2026-11-07T00:00:00Z",
            "platforms": { "windows-x86_64": { "url": "https://example.com/a.exe", "signature": "c2ln" } }
        }"#;
        let manifest = parse_manifest(old).expect("老清单能读");
        assert_eq!(manifest.notes.as_deref(), Some("旧说明"));
        assert_eq!(manifest.notes_en, None);
        assert!(manifest.select("1.0.0").unwrap().is_some());

        // 没传英文说明：生成的清单里没有 notes_en 这个键。
        let without = build_latest_json(&windows_release("c2ln")).expect("生成");
        assert!(!without.contains("notes_en"), "{without}");

        // 传了英文说明：键在，值是去掉首尾空白的英文；中文 notes 一个字不变。
        let release = ReleaseManifest {
            notes: "中文说明",
            notes_en: Some("\nEnglish notes\n"),
            ..windows_release("c2ln")
        };
        let with = build_latest_json(&release).expect("生成");
        let parsed = parse_manifest(&with).expect("读回来");
        assert_eq!(parsed.notes.as_deref(), Some("中文说明"));
        assert_eq!(parsed.notes_en.as_deref(), Some("English notes"));

        // 老客户端的解析结构只有 version / notes(body) / date(pub_date) / platforms：用一个等价的老结构读新清单，
        // 多出来的 notes_en 被忽略，读到的内容和不带英文时完全一样。
        #[derive(serde::Deserialize)]
        struct OlderManifest {
            version: String,
            #[serde(default, alias = "body")]
            notes: Option<String>,
            #[serde(default, alias = "pub_date")]
            date: Option<String>,
            platforms: BTreeMap<String, Asset>,
        }
        let older: OlderManifest = serde_json::from_str(&with).expect("老客户端读得了新清单");
        assert_eq!(older.version, "1.4.0");
        assert_eq!(older.notes.as_deref(), Some("中文说明"));
        assert_eq!(older.date.as_deref(), Some("2026-11-03T08:00:00Z"));
        assert!(older.platforms.contains_key(RELEASE_PLATFORM_KEY));

        // 空白的英文说明当没有，不会把空字符串写进清单。
        for blank in ["", "  \n "] {
            let release = ReleaseManifest {
                notes_en: Some(blank),
                ..windows_release("c2ln")
            };
            let json = build_latest_json(&release).expect("生成");
            assert!(!json.contains("notes_en"), "{json}");
        }
    }

    #[test]
    fn latest_json_generation_refuses_bad_input() {
        let ok = ReleaseManifest {
            version: "1.0.0",
            notes: "",
            notes_en: None,
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
            notes_en: None,
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
            "ZeeTerm-1.4.0-linux-x86_64.deb",
            true,
        )
        .expect("deb 签名");
        verify_signature(
            tar,
            tar_asset.signature.as_deref().unwrap(),
            &pubkey,
            "1.4.0",
            "ZeeTerm-1.4.0-linux-x86_64.tar.gz",
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
                "ZeeTerm-1.4.0-linux-x86_64.tar.gz",
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
            &[
                "linux-x86_64-deb",
                "linux-x86_64-tar",
                "linux-aarch64-deb",
                "linux-aarch64-tar"
            ]
        );
    }

    /// 新增的 Linux aarch64 键对老客户端没有影响：`platforms` 是 BTreeMap，老结构忽略多出来的键，
    /// Windows 仍只认自己的两个 key。用和 1.1.2 相同的解析结构读新清单来证明。
    #[test]
    fn new_linux_arch_keys_do_not_disturb_older_readers() {
        let assets = [
            PlatformAsset {
                key: PLATFORM_LINUX_DEB,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.deb",
                signature: "c2ln",
            },
            PlatformAsset {
                key: PLATFORM_LINUX_TAR,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                signature: "c2ln",
            },
            PlatformAsset {
                key: PLATFORM_LINUX_AARCH64_DEB,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-aarch64.deb",
                signature: "c2ln",
            },
            PlatformAsset {
                key: PLATFORM_LINUX_AARCH64_TAR,
                url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-linux-aarch64.tar.gz",
                signature: "c2ln",
            },
        ];
        let json = build_latest_json_with_platforms(&windows_release("c2ln"), &assets)
            .expect("带四种 Linux 条目");
        let parsed = parse_manifest(&json).expect("读回来");
        let selected = parsed.select("1.3.0").unwrap().expect("Windows 有更新");
        assert_eq!(selected.platform_key, RELEASE_PLATFORM_KEY);
        assert!(selected.url.ends_with("ZeeTerm-Setup-1.4.0.exe"));

        #[derive(serde::Deserialize)]
        struct OlderManifest {
            version: String,
            #[serde(default, alias = "body")]
            notes: Option<String>,
            #[serde(default, alias = "pub_date")]
            date: Option<String>,
            platforms: BTreeMap<String, Asset>,
        }
        let older: OlderManifest = serde_json::from_str(&json).expect("老客户端读得了新清单");
        assert_eq!(older.version, "1.4.0");
        assert_eq!(older.notes.as_deref(), Some("notes"));
        assert_eq!(older.date.as_deref(), Some("2026-11-03T08:00:00Z"));
        assert_eq!(older.platforms.len(), 5);
        assert!(older.platforms.contains_key(RELEASE_PLATFORM_KEY));
        assert!(older.platforms.contains_key(PLATFORM_LINUX_AARCH64_DEB));
        // 老客户端只按自己的 key 取，新增的键不会改变它选到的包。
        assert_eq!(older.platforms[RELEASE_PLATFORM_KEY].url, selected.url);
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

    fn portable_asset(signature: &str) -> PlatformAsset<'_> {
        PlatformAsset {
            key: PLATFORM_WINDOWS_PORTABLE,
            url: "https://github.com/owner/repo/releases/download/v1.4.0/ZeeTerm-1.4.0-windows-x64-portable.zip",
            signature,
        }
    }

    /// 清单里加了免安装版条目之后：老客户端（只认 `PLATFORM_KEYS`）选中的包和没有这一项时完全一样，
    /// 免安装版自己只会选 `windows-x86_64-portable`，而且签名各签各的。
    #[test]
    fn the_portable_entry_leaves_older_clients_unchanged_and_is_only_picked_by_portable() {
        let (kp, pubkey) = keypair();
        let installer = b"windows installer";
        let zip = b"portable zip bytes";
        let win_sig = sign(
            &kp,
            installer,
            &trusted_comment("1.4.0", "ZeeTerm-Setup-1.4.0.exe", 1).unwrap(),
        );
        let zip_sig = sign(
            &kp,
            zip,
            &trusted_comment("1.4.0", "ZeeTerm-1.4.0-windows-x64-portable.zip", 2).unwrap(),
        );
        let deb_sig = sign(
            &kp,
            b"deb",
            &trusted_comment("1.4.0", "ZeeTerm-1.4.0-linux-x86_64.deb", 3).unwrap(),
        );

        let without = build_latest_json(&windows_release(&win_sig)).expect("只有安装包");
        let mut extra = vec![portable_asset(&zip_sig)];
        extra.extend(linux_release(&deb_sig, &deb_sig));
        let with = build_latest_json_with_platforms(&windows_release(&win_sig), &extra)
            .expect("带免安装版条目");

        // 老客户端的解析 + 选包（`parse_manifest` 对多出来的 key 不挑）：结果逐字段一样。
        let before = parse_manifest(&without).unwrap().select("1.3.0").unwrap();
        let manifest = parse_manifest(&with).expect("带免安装版条目的清单能读");
        let after = manifest.select("1.3.0").unwrap();
        assert_eq!(before, after);
        assert_eq!(after.unwrap().platform_key, RELEASE_PLATFORM_KEY);
        assert!(
            PLATFORM_KEYS
                .iter()
                .all(|key| !EXTRA_PLATFORM_KEYS.contains(key)),
            "老客户端认的 key 和新增的 key 不能重叠"
        );

        // 免安装版只认自己的 key，选到的是 zip，签名能验过，拿安装包的签名去验 zip 必须失败。
        let picked = manifest
            .select_for("1.3.0", PORTABLE_PLATFORM_KEYS)
            .unwrap()
            .expect("有更新");
        assert_eq!(picked.platform_key, PLATFORM_WINDOWS_PORTABLE);
        assert!(
            picked
                .url
                .ends_with("/ZeeTerm-1.4.0-windows-x64-portable.zip")
        );
        let zip_name = "ZeeTerm-1.4.0-windows-x64-portable.zip";
        verify_signature(zip, &picked.signature, &pubkey, "1.4.0", zip_name, true)
            .expect("zip 签名");
        assert!(verify_signature(zip, &win_sig, &pubkey, "1.4.0", zip_name, true).is_err());
        assert!(
            verify_signature(
                installer,
                &zip_sig,
                &pubkey,
                "1.4.0",
                "ZeeTerm-Setup-1.4.0.exe",
                true
            )
            .is_err()
        );
        assert!(
            manifest
                .select_for("1.4.0", PORTABLE_PLATFORM_KEYS)
                .unwrap()
                .is_none()
        );

        // 清单里没有免安装版条目（老版本发布的）：免安装版得到"没有适用的包"，不会退而求其次装安装包。
        assert!(matches!(
            parse_manifest(&without)
                .unwrap()
                .select_for("1.3.0", PORTABLE_PLATFORM_KEYS),
            Err(UpdateError::NoPlatform)
        ));
    }

    #[test]
    fn the_portable_entry_is_validated_like_the_other_extra_entries() {
        let release = windows_release("c2ln");
        assert!(build_latest_json_with_platforms(&release, &[portable_asset("c2ln")]).is_ok());
        let unsigned = [portable_asset("  ")];
        assert!(matches!(
            build_latest_json_with_platforms(&release, &unsigned),
            Err(UpdateError::MissingSignature)
        ));
        let insecure = [PlatformAsset {
            url: "http://example.com/a.zip",
            ..portable_asset("c2ln")
        }];
        assert!(build_latest_json_with_platforms(&release, &insecure).is_err());
        let twice = [portable_asset("c2ln"), portable_asset("c2ln")];
        assert!(build_latest_json_with_platforms(&release, &twice).is_err());
        assert_eq!(PLATFORM_WINDOWS_PORTABLE, "windows-x86_64-portable");
    }

    #[test]
    fn linux_platform_generation_refuses_bad_input() {
        let release = windows_release("c2ln");
        let ok = linux_release("c2ln", "c2ln");
        assert!(build_latest_json_with_platforms(&release, &ok).is_ok());
        assert!(build_latest_json_with_platforms(&release, &[]).is_ok());

        // 不认识的 key（含 Windows 的 key：Windows 那一项只能由 `release` 自己给）。
        for key in ["linux-x86_64", "windows-x86_64", "darwin-x86_64"] {
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
        let sig_b64 = sign(&kp, data, "timestamp:1\tfile:a-setup.exe\tversion:0.3.0");
        verify_signature(data, &sig_b64, &pubkey, "0.3.0", "a-setup.exe", true)
            .expect("签名应通过");
    }

    #[test]
    fn tampered_package_is_rejected() {
        let (kp, pubkey) = keypair();
        let sig_b64 = sign(&kp, b"installer bytes", "version:0.3.0");
        let err = verify_signature(b"tampered", &sig_b64, &pubkey, "0.3.0", "a-setup.exe", true)
            .unwrap_err();
        assert!(matches!(err, UpdateError::Signature(_)));
        assert!(err.to_string().contains("对不上"), "{err}");
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "version:0.2.0");
        let err =
            verify_signature(data, &sig_b64, &pubkey, "0.3.0", "a-setup.exe", true).unwrap_err();
        assert!(matches!(err, UpdateError::SignedVersionMismatch { .. }));
    }

    #[test]
    fn missing_signed_version_is_rejected_when_required() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "timestamp:1");
        let err =
            verify_signature(data, &sig_b64, &pubkey, "0.3.0", "a-setup.exe", true).unwrap_err();
        assert!(matches!(err, UpdateError::MissingSignedVersion));
    }

    #[test]
    fn wrong_public_key_is_rejected() {
        let (kp, _) = keypair();
        let (_, other) = keypair();
        let data = b"installer bytes";
        let sig_b64 = sign(&kp, data, "version:0.3.0");
        let err =
            verify_signature(data, &sig_b64, &other, "0.3.0", "a-setup.exe", true).unwrap_err();
        assert!(matches!(err, UpdateError::Signature(_)));
        assert!(err.to_string().contains("更新密钥"), "{err}");
    }

    // ---- 文件名核对（U55） ----

    fn sign_for_file(kp: &KeyPair, data: &[u8], file: &str) -> String {
        sign(kp, data, &trusted_comment("0.3.0", file, 1).unwrap())
    }

    #[test]
    fn matching_file_name_passes() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig = sign_for_file(&kp, data, "ZeeTerm-Setup-0.3.0.exe");
        verify_signature(
            data,
            &sig,
            &pubkey,
            "0.3.0",
            "ZeeTerm-Setup-0.3.0.exe",
            true,
        )
        .expect("文件名一致应通过");
    }

    /// 清单不签名：中间人把地址换成同一版本里别的包（签名有效），文件名对不上就必须拒绝。
    #[test]
    fn a_different_file_name_is_rejected() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig = sign_for_file(&kp, data, "ZeeTerm-Setup-0.3.0.exe");
        for other in [
            // 换成另一个平台 / 架构的包名
            "ZeeTerm-Setup-0.3.0-arm64.exe",
            "ZeeTerm-0.3.0-windows-x64-portable.zip",
            // 大小写不同、多一个字符、少一个字符、首尾多空白
            "zeeterm-setup-0.3.0.exe",
            "ZeeTerm-Setup-0.3.0.exee",
            "ZeeTerm-Setup-0.3.0.ex",
            " ZeeTerm-Setup-0.3.0.exe",
            "ZeeTerm-Setup-0.3.0.exe ",
        ] {
            let err = verify_signature(data, &sig, &pubkey, "0.3.0", other, true).unwrap_err();
            assert!(
                matches!(&err, UpdateError::SignedFileMismatch { signed, .. }
                    if signed == "ZeeTerm-Setup-0.3.0.exe"),
                "{other}: {err}"
            );
        }
        // 空的期望文件名（调用方没取到）同样不能放行，且不是"文件名不一致"。
        let err = verify_signature(data, &sig, &pubkey, "0.3.0", "", true).unwrap_err();
        assert!(
            matches!(
                err,
                UpdateError::Signature(SignatureProblem::NoExpectedFileName)
            ),
            "{err}"
        );
    }

    #[test]
    fn missing_signed_file_is_rejected_when_required() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        // 有版本、没有文件名。
        let sig = sign(&kp, data, "timestamp:1\tversion:0.3.0");
        let err = verify_signature(data, &sig, &pubkey, "0.3.0", "a-setup.exe", true).unwrap_err();
        assert!(matches!(err, UpdateError::MissingSignedFile), "{err}");
    }

    /// `require_signed_version == false` 是给更早、没写这两项的签名留的宽松路径：缺的项不查，
    /// 写了的项仍然必须对得上。
    #[test]
    fn lenient_path_skips_missing_fields_but_still_checks_present_ones() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let bare = sign(&kp, data, "timestamp:1");
        verify_signature(data, &bare, &pubkey, "0.3.0", "a-setup.exe", false)
            .expect("什么都没写的旧签名在宽松路径上放行");
        let versioned = sign(&kp, data, "timestamp:1\tversion:0.3.0");
        verify_signature(data, &versioned, &pubkey, "0.3.0", "a-setup.exe", false)
            .expect("只有版本、没有文件名：宽松路径放行");

        let named = sign(&kp, data, "timestamp:1\tfile:b-setup.exe");
        let err =
            verify_signature(data, &named, &pubkey, "0.3.0", "a-setup.exe", false).unwrap_err();
        assert!(
            matches!(err, UpdateError::SignedFileMismatch { .. }),
            "写了文件名就必须对得上：{err}"
        );
        verify_signature(data, &named, &pubkey, "0.3.0", "b-setup.exe", false)
            .expect("写了且一致就放行");
    }

    /// 版本对不上时先报版本（和以前一致），不会被文件名的结果盖掉。
    #[test]
    fn version_is_still_checked_before_the_file_name() {
        let (kp, pubkey) = keypair();
        let data = b"installer bytes";
        let sig = sign(&kp, data, "timestamp:1\tfile:other.exe\tversion:0.2.0");
        let err = verify_signature(data, &sig, &pubkey, "0.3.0", "a-setup.exe", true).unwrap_err();
        assert!(
            matches!(err, UpdateError::SignedVersionMismatch { .. }),
            "{err}"
        );
    }

    /// 非预先哈希（`Ed`）的旧格式签名不再接受；预先哈希（`ED`）的照常通过。
    /// 旧格式样本是 minisign 官方实现自带的测试向量（公钥、数据 `test` 与签名都是公开的测试数据）：
    /// 先用库自己的 `allow_legacy = true` 证明它本身是一个有效的旧格式签名，再证明我们的验签拒绝它。
    #[test]
    fn legacy_non_prehashed_signatures_are_rejected() {
        let pk_text = "untrusted comment: minisign public key E7620F1842B4E81F\n\
            RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let sig_text = "untrusted comment: signature from minisign secret key\n\
            RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\n\
            trusted comment: timestamp:1555779966\tfile:test\n\
            QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
        let data = b"test";

        let public_key = PublicKey::decode(pk_text).expect("公钥");
        let signature = Signature::decode(sig_text).expect("签名");
        public_key
            .verify(data, &signature, true)
            .expect("样本本身是有效的旧格式签名（允许旧格式时能通过）");

        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let err = verify_signature(data, &b64(sig_text), &b64(pk_text), "0.3.0", "test", false)
            .unwrap_err();
        assert!(
            matches!(
                err,
                UpdateError::Signature(SignatureProblem::UnsupportedAlgorithm)
            ),
            "{err}"
        );
        // 宽松路径（`false`）也不例外：旧格式开关和 `require_signed_version` 无关。
        let err = verify_signature(data, &b64(sig_text), &b64(pk_text), "0.3.0", "test", true)
            .unwrap_err();
        assert!(matches!(err, UpdateError::Signature(_)), "{err}");

        // 同样的数据用预先哈希的签名则照常通过。
        let (kp, pubkey) = keypair();
        let sig = sign_for_file(&kp, data, "test");
        verify_signature(data, &sig, &pubkey, "0.3.0", "test", true).expect("预哈希签名通过");
    }

    /// 1.0.9 的 `latest.json` 里线上签名的回归：全是预先哈希格式，trusted comment 带 `file:` 和
    /// `version:`，文件名与下载地址的最后一段完全一致；换成别的平台的包名、改大小写、多一个字符都过不了。
    /// 这里只验证 trusted comment 的解析和文件名 / 版本核对（没有安装包本体，整包验签用上面自己生成的密钥）。
    #[test]
    fn released_1_0_9_signatures_bind_the_published_file_names() {
        // 1.0.9 正式发布的 `latest.json` 里各平台的签名文本和下载地址（只有这两样，没有安装包本体）。
        const RELEASED_1_0_9: [(&str, &str, &str); 4] = [
            (
                "windows-x86_64",
                "https://github.com/MoeShinX/ZeeTerm/releases/download/v1.0.9/ZeeTerm-Setup-1.0.9.exe",
                "dW50cnVzdGVkIGNvbW1lbnQ6IFplZVRlcm0gdXBkYXRlIHNpZ25hdHVyZQpSVVF3bXUrWDhtV1Vqbkp5VUlmcEZrcXBzcDk2NVZIaDV2OHpMOVRCZUNWendaSXY5aENDR211RDZDYXgyMDViYXMyU1dhNCtUTDFvdGc1L3FZSElwSkJBaitVUUxLeitzQXM9CnRydXN0ZWQgY29tbWVudDogdGltZXN0YW1wOjE3OTUwNjk1MDkJZmlsZTpaZWVUZXJtLVNldHVwLTEuMC45LmV4ZQl2ZXJzaW9uOjEuMC45CmlWN1FvdTRmNUpLSGpqSmxrVWJhQVlFTWtwSHVYWmtsV1VJbmxSRGZqV2NZUEg0eGVWdWJzLyt3VllMQnhaNmV4b2padGxobEswbTFVTC81UzNqQkR3PT0K",
            ),
            (
                "windows-x86_64-portable",
                "https://github.com/MoeShinX/ZeeTerm/releases/download/v1.0.9/ZeeTerm-1.0.9-windows-x64-portable.zip",
                "dW50cnVzdGVkIGNvbW1lbnQ6IFplZVRlcm0gdXBkYXRlIHNpZ25hdHVyZQpSVVF3bXUrWDhtV1VqbE5JSDJjREZnSWVQb2lqS3VrNTI4TUhCWFlhMU5ETm1PQ1lBSm9idEpOdnZUTThPZXJsYmFGMms4M2ZodEE0Q3ZJajBlWTdiUGp4YUc2TzA3cm9md009CnRydXN0ZWQgY29tbWVudDogdGltZXN0YW1wOjE3OTUwNjk1MDkJZmlsZTpaZWVUZXJtLTEuMC45LXdpbmRvd3MteDY0LXBvcnRhYmxlLnppcAl2ZXJzaW9uOjEuMC45CnIwOHo3c2RLeEw4Si9Hb204bUFRU3lzT3crS3pBSC8wQlhqQU9pcm9yMlJyZjRGS0FWWDJjem9FeTZqZ2xDTDhuYVRjMkI5RWQySzQwSU9aTTAzRERnPT0K",
            ),
            (
                "linux-x86_64-deb",
                "https://github.com/MoeShinX/ZeeTerm/releases/download/v1.0.9/ZeeTerm-1.0.9-linux-x86_64.deb",
                "dW50cnVzdGVkIGNvbW1lbnQ6IFplZVRlcm0gdXBkYXRlIHNpZ25hdHVyZQpSVVF3bXUrWDhtV1Vqa2h1WnZnM2pLdjV5V01nSUpTQmg2TXZGWWdnVFFJbGJWdHo2SThyd0JxS3U3UU9YVW1xNHZaOS8rdjlMVGhUemxzMEVvOWFSZVJmTGkyU0xVSU4wQVE9CnRydXN0ZWQgY29tbWVudDogdGltZXN0YW1wOjE3OTUwNjk1MDkJZmlsZTpaZWVUZXJtLTEuMC45LWxpbnV4LXg4Nl82NC5kZWIJdmVyc2lvbjoxLjAuOQpFMU93VXR3aW9qVldZUDdzL091SVE5WkFOb0lBS1hSUWd5eDM0Mkc0ZzBBUkVwTHJlSGVWMk1XSTJEREppSGFNaWd5VnBBNHRyYkkrWHpNMGg4Z3VEdz09Cg==",
            ),
            (
                "linux-x86_64-tar",
                "https://github.com/MoeShinX/ZeeTerm/releases/download/v1.0.9/ZeeTerm-1.0.9-linux-x86_64.tar.gz",
                "dW50cnVzdGVkIGNvbW1lbnQ6IFplZVRlcm0gdXBkYXRlIHNpZ25hdHVyZQpSVVF3bXUrWDhtV1VqbDlxR1R0M2M5YzlneHp4bzB1a2c0aEU0dmR5SndhUFp3YW5iMWhyS2hTTmM0eGNmdXlsMUtZSk10OGk0bVIxRzJhVS9ScHRLKzZxSWtmY0JOMnZ6dzQ9CnRydXN0ZWQgY29tbWVudDogdGltZXN0YW1wOjE3OTUwNjk1MTAJZmlsZTpaZWVUZXJtLTEuMC45LWxpbnV4LXg4Nl82NC50YXIuZ3oJdmVyc2lvbjoxLjAuOQpoYXJDQ3JkaEFja2FZKzBEMXZsMGxuNG52cXRndW00cmltby9MQVRXZ3N4RHE1Z1M0VTdTZ3pDV0ltem4weWhWM1BEUlo4M0xIbWdsOHpUSGlZTlZCQT09Cg==",
            ),
        ];

        assert_eq!(
            file_name_from_url(RELEASED_1_0_9[0].1).as_deref(),
            Some("ZeeTerm-Setup-1.0.9.exe")
        );
        let names: Vec<String> = RELEASED_1_0_9
            .iter()
            .map(|(_, url, _)| file_name_from_url(url).expect("发布地址里有文件名"))
            .collect();
        for (index, (key, _url, sig_b64)) in RELEASED_1_0_9.iter().enumerate() {
            let text = decode_text(sig_b64).unwrap();
            let signature = Signature::decode(&text).expect("线上签名能解码");
            // 签名第二行解码后头两个字节是 `ED`：预先哈希格式，所以关掉旧格式兼容不影响线上的包。
            let raw = base64::engine::general_purpose::STANDARD
                .decode(text.lines().nth(1).unwrap())
                .unwrap();
            assert_eq!(&raw[..2], b"ED", "{key}");

            let comment = signature.trusted_comment();
            assert_eq!(signed_file(comment), Some(names[index].as_str()), "{key}");
            verify_signed_version(comment, "1.0.9", true).expect(key);
            verify_signed_file(comment, &names[index], true).expect(key);

            // 同一个版本里别的平台的包名一律拒绝。
            for (other_index, other) in names.iter().enumerate() {
                if other_index != index {
                    let err = verify_signed_file(comment, other, true).unwrap_err();
                    assert!(
                        matches!(err, UpdateError::SignedFileMismatch { .. }),
                        "{key} vs {other}: {err}"
                    );
                }
            }
            for wrong in [
                names[index].to_uppercase(),
                format!("{}x", names[index]),
                names[index][1..].to_owned(),
            ] {
                assert!(
                    verify_signed_file(comment, &wrong, true).is_err(),
                    "{wrong}"
                );
            }
        }
    }

    #[test]
    fn download_url_file_names() {
        let ok = |url: &str| file_name_from_url(url);
        assert_eq!(
            ok("https://example.com/a/b/ZeeTerm-Setup-1.0.9.exe").as_deref(),
            Some("ZeeTerm-Setup-1.0.9.exe")
        );
        // 查询串和片段去掉，哪怕里面有 `/`。
        assert_eq!(ok("https://h/a/x.exe?token=1").as_deref(), Some("x.exe"));
        assert_eq!(ok("https://h/a/x.exe#frag").as_deref(), Some("x.exe"));
        assert_eq!(
            ok("https://h/a/x.exe?p=/y/z.exe#q/r").as_deref(),
            Some("x.exe")
        );
        // 百分号解码（大小写十六进制都认；UTF-8 多字节）。
        assert_eq!(
            ok("https://h/Zee%20Term.exe").as_deref(),
            Some("Zee Term.exe")
        );
        assert_eq!(
            ok("https://h/%E4%B8%AD%e6%96%87.exe").as_deref(),
            Some("中文.exe")
        );
        // 本地文件地址（测试用）。
        assert_eq!(ok("file:///C:/tmp/a.exe").as_deref(), Some("a.exe"));
        // 末尾斜杠、没有路径、只有查询串：取不出文件名。
        for url in [
            "https://h/dir/",
            "https://h/",
            "https://h",
            "https://h?x=/a.exe",
            "https://h#/a.exe",
            "https://h/a/..",
            "https://h/a/.",
            "https://h/a/%2e%2e",
            "",
            "a.exe",
            "/a.exe",
        ] {
            assert_eq!(ok(url), None, "{url}");
        }
        // 解码后含路径分隔符、控制字符，或转义不完整 / 不是 UTF-8：当作取不出。
        for url in [
            "https://h/a%2Fb.exe",
            "https://h/a%5Cb.exe",
            "https://h/a\\b.exe",
            "https://h/a%00b.exe",
            "https://h/a%0Ab.exe",
            "https://h/a%zzb.exe",
            "https://h/a%2",
            "https://h/a%",
            "https://h/a%E4.exe",
        ] {
            assert_eq!(ok(url), None, "{url}");
        }
    }

    #[test]
    fn file_name_in_errors_is_cleaned_and_shortened() {
        assert_eq!(display_file_name("a\u{1b}[31mb\n.exe"), "a[31mb.exe");
        let long = "x".repeat(500);
        let shown = display_file_name(&long);
        assert!(shown.chars().count() < 100, "{}", shown.len());
        assert!(shown.ends_with('…'));
        let (kp, pubkey) = keypair();
        let sig = sign(
            &kp,
            b"d",
            &format!("timestamp:1\tfile:{long}\tversion:0.3.0"),
        );
        let err = verify_signature(b"d", &sig, &pubkey, "0.3.0", "a.exe", true).unwrap_err();
        assert!(err.to_string().chars().count() < 250, "{err}");
    }
}
