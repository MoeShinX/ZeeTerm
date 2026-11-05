//! 更新器错误的界面文案。
//!
//! `zssh-update` 里的 `UpdateError` / `InstallError` / `SignatureProblem` 本来就是类型，界面按类型和
//! 当前语言出词；它们的中文 `Display` 留给日志和测试。文件路径、版本号、签名库给的英文原文作为参数
//! 原样带上。

use zssh_update::install::InstallError;
use zssh_update::{SignatureProblem, UpdateError};

use crate::i18n::{Lang, Msg, current};

#[derive(Clone, Copy, Debug)]
enum UpdateTextMsg {
    Json,
    BadVersion,
    NoPlatform,
    MissingSignature,
    Base64,
    Signature,
    MissingSignedVersion,
    SignedVersionMismatch,
    ProblemMismatch,
    ProblemWrongKey,
    ProblemBadEncoding,
    ProblemUnsupportedAlgorithm,
    ProblemHttpsRequired,
    ProblemControlCharacters,
    EmptyVersion,
    MissingInstaller,
    NotAFile,
    NotExe,
    OutsideCache,
    Path,
    Unreadable,
    HashMismatch,
}

impl Msg for UpdateTextMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Json => "更新说明格式错误：{err}",
            Self::BadVersion => "版本号不合法：{version}",
            Self::NoPlatform => "没有适用于当前平台的安装包",
            Self::MissingSignature => "安装包缺少签名",
            Self::Base64 => "签名编码错误：{err}",
            Self::Signature => "签名校验未通过：{problem}",
            Self::MissingSignedVersion => "签名里没有版本号，拒绝安装",
            Self::SignedVersionMismatch => "签名版本 {signed} 与公告版本 {announced} 不一致",
            Self::ProblemMismatch => "安装包内容与签名对不上（文件可能被改动过，或下载不完整）",
            Self::ProblemWrongKey => "签名不是用 ZeeTerm 的更新密钥签的",
            Self::ProblemBadEncoding => "签名或公钥的格式不对",
            Self::ProblemUnsupportedAlgorithm => "不支持的签名算法",
            Self::ProblemHttpsRequired => "安装包地址必须是 https：{url}",
            Self::ProblemControlCharacters => "安装包文件名里有控制字符",
            Self::EmptyVersion => "安装包版本为空",
            Self::MissingInstaller => "安装包不存在",
            Self::NotAFile => "安装包不是文件",
            Self::NotExe => "安装包不是 .exe",
            Self::OutsideCache => "安装包不在更新缓存目录内，拒绝执行",
            Self::Path => "安装包路径无法解析：{err}",
            Self::Unreadable => "读取安装包失败：{err}",
            Self::HashMismatch => "安装包内容与验签时不一致",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Json => "The update notes are malformed: {err}",
            Self::BadVersion => "Invalid version number: {version}",
            Self::NoPlatform => "There is no installer for this platform",
            Self::MissingSignature => "The installer has no signature",
            Self::Base64 => "The signature encoding is invalid: {err}",
            Self::Signature => "Signature check failed: {problem}",
            Self::MissingSignedVersion => {
                "The signature carries no version, so installation is refused"
            }
            Self::SignedVersionMismatch => {
                "The signed version {signed} does not match the announced version {announced}"
            }
            Self::ProblemMismatch => {
                "The installer does not match its signature (the file may have been altered, or the download is incomplete)"
            }
            Self::ProblemWrongKey => "The signature was not made with ZeeTerm's update key",
            Self::ProblemBadEncoding => "The signature or public key format is invalid",
            Self::ProblemUnsupportedAlgorithm => "Unsupported signature algorithm",
            Self::ProblemHttpsRequired => "The installer URL must be https: {url}",
            Self::ProblemControlCharacters => "The installer file name contains control characters",
            Self::EmptyVersion => "The installer version is empty",
            Self::MissingInstaller => "The installer does not exist",
            Self::NotAFile => "The installer is not a file",
            Self::NotExe => "The installer is not an .exe",
            Self::OutsideCache => {
                "The installer is outside the update cache folder, so it will not be run"
            }
            Self::Path => "Cannot resolve the installer path: {err}",
            Self::Unreadable => "Failed to read the installer: {err}",
            Self::HashMismatch => "The installer contents differ from what was verified",
        }
    }
}

fn fill(lang: Lang, msg: UpdateTextMsg, params: &[(&str, &str)]) -> String {
    let mut text = match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    }
    .to_owned();
    for (name, value) in params {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

fn problem_in(lang: Lang, problem: &SignatureProblem) -> String {
    let plain = |msg: UpdateTextMsg| fill(lang, msg, &[]);
    match problem {
        SignatureProblem::Mismatch => plain(UpdateTextMsg::ProblemMismatch),
        SignatureProblem::WrongKey => plain(UpdateTextMsg::ProblemWrongKey),
        SignatureProblem::BadEncoding => plain(UpdateTextMsg::ProblemBadEncoding),
        SignatureProblem::UnsupportedAlgorithm => plain(UpdateTextMsg::ProblemUnsupportedAlgorithm),
        SignatureProblem::HttpsRequired { url } => {
            fill(lang, UpdateTextMsg::ProblemHttpsRequired, &[("url", url)])
        }
        SignatureProblem::ControlCharacters => plain(UpdateTextMsg::ProblemControlCharacters),
        // 签名库给的原文（英文），原样显示。
        SignatureProblem::Other(text) => text.clone(),
    }
}

/// 更新清单 / 验签的错误。
pub(crate) fn update_error(err: &UpdateError) -> String {
    update_error_in(current(), err)
}

pub(crate) fn update_error_in(lang: Lang, err: &UpdateError) -> String {
    let plain = |msg: UpdateTextMsg| fill(lang, msg, &[]);
    match err {
        UpdateError::Json(inner) => fill(lang, UpdateTextMsg::Json, &[("err", &inner.to_string())]),
        UpdateError::BadVersion(version) => {
            fill(lang, UpdateTextMsg::BadVersion, &[("version", version)])
        }
        UpdateError::NoPlatform => plain(UpdateTextMsg::NoPlatform),
        UpdateError::MissingSignature => plain(UpdateTextMsg::MissingSignature),
        UpdateError::Base64(inner) => fill(lang, UpdateTextMsg::Base64, &[("err", inner)]),
        UpdateError::Signature(problem) => fill(
            lang,
            UpdateTextMsg::Signature,
            &[("problem", &problem_in(lang, problem))],
        ),
        UpdateError::MissingSignedVersion => plain(UpdateTextMsg::MissingSignedVersion),
        UpdateError::SignedVersionMismatch { signed, announced } => fill(
            lang,
            UpdateTextMsg::SignedVersionMismatch,
            &[("signed", signed), ("announced", announced)],
        ),
    }
}

/// 安装计划 / 复验的错误。
pub(crate) fn install_error(err: &InstallError) -> String {
    install_error_in(current(), err)
}

pub(crate) fn install_error_in(lang: Lang, err: &InstallError) -> String {
    let plain = |msg: UpdateTextMsg| fill(lang, msg, &[]);
    match err {
        InstallError::EmptyVersion => plain(UpdateTextMsg::EmptyVersion),
        InstallError::MissingInstaller => plain(UpdateTextMsg::MissingInstaller),
        InstallError::NotAFile => plain(UpdateTextMsg::NotAFile),
        InstallError::NotExe => plain(UpdateTextMsg::NotExe),
        InstallError::OutsideCache => plain(UpdateTextMsg::OutsideCache),
        InstallError::Path(inner) => fill(lang, UpdateTextMsg::Path, &[("err", inner)]),
        InstallError::Unreadable(inner) => fill(lang, UpdateTextMsg::Unreadable, &[("err", inner)]),
        InstallError::HashMismatch => plain(UpdateTextMsg::HashMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }

    #[test]
    fn update_errors_match_the_core_in_chinese_and_are_english_otherwise() {
        let json_error = zssh_update::parse_manifest("{").unwrap_err();
        let errors = vec![
            json_error,
            UpdateError::BadVersion("v1".into()),
            UpdateError::NoPlatform,
            UpdateError::MissingSignature,
            UpdateError::Base64("bad padding".into()),
            UpdateError::Signature(SignatureProblem::Mismatch),
            UpdateError::Signature(SignatureProblem::WrongKey),
            UpdateError::Signature(SignatureProblem::BadEncoding),
            UpdateError::Signature(SignatureProblem::UnsupportedAlgorithm),
            UpdateError::Signature(SignatureProblem::HttpsRequired {
                url: "http://example.com/a.exe".into(),
            }),
            UpdateError::Signature(SignatureProblem::ControlCharacters),
            UpdateError::Signature(SignatureProblem::Other("some library text".into())),
            UpdateError::MissingSignedVersion,
            UpdateError::SignedVersionMismatch {
                signed: "1.0.0".into(),
                announced: "1.0.1".into(),
            },
        ];
        for err in errors {
            assert_eq!(update_error_in(Lang::Zh, &err), err.to_string(), "{err:?}");
            let en = update_error_in(Lang::En, &err);
            // JSON 解析器的原文是英文参数，不会含汉字。
            assert!(!has_cjk(&en), "{err:?} → {en}");
        }
    }

    #[test]
    fn install_errors_match_the_core_in_chinese_and_are_english_otherwise() {
        let errors = [
            InstallError::EmptyVersion,
            InstallError::MissingInstaller,
            InstallError::NotAFile,
            InstallError::NotExe,
            InstallError::OutsideCache,
            InstallError::Path(r"C:\cache\x".into()),
            InstallError::Unreadable("access is denied".into()),
            InstallError::HashMismatch,
        ];
        for err in errors {
            assert_eq!(install_error_in(Lang::Zh, &err), err.to_string(), "{err:?}");
            let en = install_error_in(Lang::En, &err);
            assert!(!has_cjk(&en), "{err:?} → {en}");
        }
        let path = install_error_in(Lang::En, &InstallError::Path(r"C:\cache\x".into()));
        assert_eq!(path, r"Cannot resolve the installer path: C:\cache\x");
    }
}
