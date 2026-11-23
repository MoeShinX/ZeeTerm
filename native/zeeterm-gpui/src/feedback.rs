//! 反馈入口（V2-U62）：拼出 GitHub「新建 issue」页的地址，版本、系统、安装方式预先填好。
//!
//! ZeeTerm 自己不发任何网络请求：这里只产出一个地址，由调用方交给系统浏览器打开，用户在网页上看过、改过再自己提交。
//! 参数名就是 `.github/ISSUE_TEMPLATE/*.yml` 里的字段 id（GitHub 用 `?字段id=值` 预填表单），
//! 下拉框的值要和模板里的选项文字完全一致——两边由测试互相核对，改一边忘了另一边会在测试里报出来。
//!
//! 预填的系统信息只有版本号、系统名（和 Windows 的 build 号）、安装方式，不含计算机名、用户名、路径。

use crate::product;

/// 模板里「安装方式」下拉框的选项文字（和 `bug_report.yml` 一字不差）。
// i18n-allow: 与 GitHub 模板里的选项文字逐字对应，不是界面文案
pub(crate) const INSTALL_INSTALLER: &str = "安装版 / Installer";
// i18n-allow: 同上
pub(crate) const INSTALL_PORTABLE: &str = "免安装版 / Portable";
pub(crate) const INSTALL_LINUX_DEB: &str = "Linux deb";
pub(crate) const INSTALL_LINUX_TAR: &str = "Linux tar";
pub(crate) const INSTALL_MACOS_ZIP: &str = "macOS zip";
// i18n-allow: 同上
pub(crate) const INSTALL_SELF_BUILT: &str = "自己编译 / Built from source";

/// 整个地址（百分号编码之后）最多这么多个字符：再长，有的浏览器 / 代理 / GitHub 会直接拒绝。
pub(crate) const MAX_URL_CHARS: usize = 6000;

/// 缩短后加在末尾的省略号。
const ELLIPSIS: char = '…';

/// 版本号（`v1.1.2`）：不带免安装版标记，标记放在「安装方式」里。
pub(crate) fn version() -> String {
    crate::about_view::version_text()
}

/// 系统信息的文字。Windows 是 `Windows <build>`（读不到 build 号就只有 `Windows`），其他平台是 `系统 架构`。
/// 只放系统名和 build 号，不放计算机名、用户名、区域设置之外的任何信息。
pub(crate) fn os_text() -> String {
    os_text_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        zeeterm_platform_windows::os_build::windows_build_number(),
    )
}

pub(crate) fn os_text_for(os: &str, arch: &str, windows_build: Option<u32>) -> String {
    if os == "windows" {
        return match windows_build {
            Some(build) => format!("Windows {build}"),
            None => "Windows".to_owned(),
        };
    }
    format!("{os} {arch}")
}

/// 这一份程序是怎么装上的，取值是模板下拉框的选项文字；推不出来是 `None`（那就不带这个参数）。
pub(crate) fn install_kind() -> Option<&'static str> {
    let exe = std::env::current_exe().ok();
    install_kind_for(
        std::env::consts::OS,
        cfg!(debug_assertions),
        crate::portable::is_portable(),
        exe.as_deref(),
    )
}

/// `dev_build`：调试构建只会是自己编译出来的。Linux 的 deb 把程序放在 `/usr/lib/zeeterm`，tar.gz 解压到哪都行，
/// 所以在 `/usr/` 下的算 deb，其他算 tar。
pub(crate) fn install_kind_for(
    os: &str,
    dev_build: bool,
    portable: bool,
    exe: Option<&std::path::Path>,
) -> Option<&'static str> {
    if dev_build {
        return Some(INSTALL_SELF_BUILT);
    }
    match os {
        "windows" if portable => Some(INSTALL_PORTABLE),
        "windows" => Some(INSTALL_INSTALLER),
        "macos" => Some(INSTALL_MACOS_ZIP),
        "linux" => {
            let packaged = exe.is_some_and(|path| path.starts_with("/usr/"));
            Some(if packaged {
                INSTALL_LINUX_DEB
            } else {
                INSTALL_LINUX_TAR
            })
        }
        _ => None,
    }
}

/// 百分号编码：只留 RFC 3986 的非保留字符，其余（含中文、空格、换行）按 UTF-8 字节逐个编成 `%XX`。
pub(crate) fn percent_encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// 「新建 issue」页的地址：`template` 是模板文件名，空值的参数不带。
pub(crate) fn issue_url(template: &str, params: &[(&str, &str)]) -> String {
    let mut url = format!(
        "{}/issues/new?template={}",
        product::REPO_URL,
        percent_encode(template)
    );
    for (name, value) in params {
        if value.is_empty() {
            continue;
        }
        url.push('&');
        url.push_str(&percent_encode(name));
        url.push('=');
        url.push_str(&percent_encode(value));
    }
    url
}

/// 「报告问题」的地址。`logs` 是要预填进「日志」框的文字（已经清洗过），没有就不带；
/// 整个地址超过 [`MAX_URL_CHARS`] 时把 `logs` 末尾截短（加 `…`），其余参数不动。
pub(crate) fn bug_url(
    version: &str,
    os: &str,
    install: Option<&str>,
    logs: Option<&str>,
) -> String {
    let build = |logs: Option<&str>| {
        issue_url(
            "bug_report.yml",
            &[
                ("version", version),
                ("os", os),
                ("install", install.unwrap_or_default()),
                ("logs", logs.unwrap_or_default()),
            ],
        )
    };
    let Some(logs) = logs else {
        return build(None);
    };
    let full = build(Some(logs));
    if full.chars().count() <= MAX_URL_CHARS {
        return full;
    }
    // 按字符数二分找到放得下的最长前缀（编码后的长度随前缀单调增加）。
    let chars: Vec<char> = logs.chars().collect();
    let with_prefix = |count: usize| {
        let mut text: String = chars[..count].iter().collect();
        text.push(ELLIPSIS);
        build(Some(&text))
    };
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if with_prefix(mid).chars().count() <= MAX_URL_CHARS {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    with_prefix(low)
}

/// 「功能建议」的地址。
pub(crate) fn feature_url(version: &str) -> String {
    issue_url("feature_request.yml", &[("version", version)])
}

/// 关于页「报告问题」按钮要打开的地址（不带日志）。
pub(crate) fn bug_url_for_this_build() -> String {
    bug_url(&version(), &os_text(), install_kind(), None)
}

/// 关于页「功能建议」按钮要打开的地址。
pub(crate) fn feature_url_for_this_build() -> String {
    feature_url(&version())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const BUG_TEMPLATE: &str = include_str!("../../../.github/ISSUE_TEMPLATE/bug_report.yml");
    const FEATURE_TEMPLATE: &str =
        include_str!("../../../.github/ISSUE_TEMPLATE/feature_request.yml");

    /// 模板里所有字段的 id（`id: xxx` 行）。
    fn field_ids(template: &str) -> Vec<String> {
        template
            .lines()
            .filter_map(|line| line.trim().strip_prefix("id: "))
            .map(|id| id.trim().to_owned())
            .collect()
    }

    /// 下拉框 `install` 的选项文字（`options:` 之后连续的 `- xxx` 行）。
    fn install_options(template: &str) -> Vec<String> {
        let mut options = Vec::new();
        let mut inside = false;
        for line in template.lines() {
            let trimmed = line.trim();
            if trimmed == "options:" {
                inside = true;
            } else if inside {
                match trimmed.strip_prefix("- ") {
                    Some(option) => options.push(option.trim().to_owned()),
                    None => break,
                }
            }
        }
        options
    }

    #[test]
    fn percent_encoding_keeps_unreserved_and_escapes_everything_else() {
        assert_eq!(percent_encode("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(
            percent_encode("a&b=c?d#e/f+g%h"),
            "a%26b%3Dc%3Fd%23e%2Ff%2Bg%25h"
        );
        assert_eq!(percent_encode("l1\nl2\r\n"), "l1%0Al2%0D%0A");
        // 中文按 UTF-8 字节编，省略号也一样。
        assert_eq!(percent_encode("张 三"), "%E5%BC%A0%20%E4%B8%89");
        assert_eq!(percent_encode("…"), "%E2%80%A6");
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn the_bug_url_carries_version_os_and_install() {
        let url = bug_url("v1.2.3", "Windows 22631", Some(INSTALL_PORTABLE), None);
        assert_eq!(
            url,
            "https://github.com/MoeShinX/ZeeTerm/issues/new?template=bug_report.yml\
             &version=v1.2.3&os=Windows%2022631\
             &install=%E5%85%8D%E5%AE%89%E8%A3%85%E7%89%88%20%2F%20Portable"
        );
    }

    #[test]
    fn missing_parameters_are_left_out_rather_than_sent_empty() {
        let url = bug_url("v1.2.3", "linux x86_64", None, None);
        assert!(!url.contains("install="), "{url}");
        assert!(!url.contains("logs="), "{url}");
        assert!(url.ends_with("&os=linux%20x86_64"), "{url}");
        let empty = bug_url("", "", None, Some(""));
        assert_eq!(
            empty,
            "https://github.com/MoeShinX/ZeeTerm/issues/new?template=bug_report.yml"
        );
        assert_eq!(
            feature_url("v1.2.3"),
            "https://github.com/MoeShinX/ZeeTerm/issues/new?template=feature_request.yml&version=v1.2.3"
        );
        assert!(!feature_url("").contains("version="));
    }

    #[test]
    fn logs_are_encoded_into_the_url() {
        let url = bug_url("v1", "os", None, Some("a b\nc&d=e"));
        assert!(url.ends_with("&logs=a%20b%0Ac%26d%3De"), "{url}");
    }

    #[test]
    fn a_long_log_is_cut_with_an_ellipsis_to_fit_the_limit() {
        let logs = "错误".repeat(5000);
        let url = bug_url(
            "v1.2.3",
            "Windows 22631",
            Some(INSTALL_INSTALLER),
            Some(&logs),
        );
        assert!(
            url.chars().count() <= MAX_URL_CHARS,
            "{}",
            url.chars().count()
        );
        assert!(url.ends_with("%E2%80%A6"), "截断处要有省略号");
        // 尽量用满：再多一个字就超了。
        assert!(
            url.chars().count() > MAX_URL_CHARS - 20,
            "{}",
            url.chars().count()
        );
        // 放得下的不动。
        let short = bug_url("v1", "os", None, Some("短"));
        assert!(!short.ends_with("%E2%80%A6"));
    }

    #[test]
    fn os_text_is_only_the_system_name_and_build() {
        assert_eq!(
            os_text_for("windows", "x86_64", Some(22631)),
            "Windows 22631"
        );
        assert_eq!(os_text_for("windows", "x86_64", None), "Windows");
        assert_eq!(os_text_for("linux", "x86_64", None), "linux x86_64");
        assert_eq!(os_text_for("macos", "aarch64", Some(1)), "macos aarch64");
        let here = os_text();
        assert!(!here.is_empty());
        // 不带计算机名、用户名。
        for secret in ["COMPUTERNAME", "USERNAME", "USER"] {
            if let Ok(value) = std::env::var(secret)
                && value.chars().count() >= 4
            {
                assert!(
                    !here.to_lowercase().contains(&value.to_lowercase()),
                    "系统信息里不该有 {secret}"
                );
            }
        }
    }

    #[test]
    fn the_install_kind_follows_platform_and_portable_marker() {
        let none = None;
        assert_eq!(
            install_kind_for("windows", false, false, none),
            Some(INSTALL_INSTALLER)
        );
        assert_eq!(
            install_kind_for("windows", false, true, none),
            Some(INSTALL_PORTABLE)
        );
        assert_eq!(
            install_kind_for("macos", false, false, none),
            Some(INSTALL_MACOS_ZIP)
        );
        assert_eq!(
            install_kind_for(
                "linux",
                false,
                false,
                Some(Path::new("/usr/lib/zeeterm/zeeterm"))
            ),
            Some(INSTALL_LINUX_DEB)
        );
        assert_eq!(
            install_kind_for(
                "linux",
                false,
                false,
                Some(Path::new("/opt/zeeterm/zeeterm"))
            ),
            Some(INSTALL_LINUX_TAR)
        );
        assert_eq!(
            install_kind_for("linux", false, false, none),
            Some(INSTALL_LINUX_TAR)
        );
        // 调试构建一律是自己编译的；认不出的系统不带这个参数。
        assert_eq!(
            install_kind_for("windows", true, false, none),
            Some(INSTALL_SELF_BUILT)
        );
        assert_eq!(install_kind_for("freebsd", false, false, none), None);
    }

    #[test]
    fn install_values_match_the_dropdown_options_in_the_template() {
        let options = install_options(BUG_TEMPLATE);
        for value in [
            INSTALL_INSTALLER,
            INSTALL_PORTABLE,
            INSTALL_LINUX_DEB,
            INSTALL_LINUX_TAR,
            INSTALL_MACOS_ZIP,
            INSTALL_SELF_BUILT,
        ] {
            assert!(
                options.iter().any(|option| option == value),
                "{value} 不在模板选项里：{options:?}"
            );
        }
        assert_eq!(options.len(), 6, "模板里多了选项：{options:?}");
    }

    #[test]
    fn url_parameter_names_are_field_ids_in_the_templates() {
        let bug_ids = field_ids(BUG_TEMPLATE);
        for id in [
            "version", "os", "install", "steps", "expected", "actual", "logs",
        ] {
            assert!(
                bug_ids.iter().any(|found| found == id),
                "bug_report.yml 缺字段 {id}"
            );
        }
        let feature_ids = field_ids(FEATURE_TEMPLATE);
        for id in ["problem", "proposal", "alternatives", "version"] {
            assert!(
                feature_ids.iter().any(|found| found == id),
                "feature_request.yml 缺字段 {id}"
            );
        }
        // 地址里实际用到的参数名都在对应模板里。
        let bug = bug_url("v1", "os", Some(INSTALL_INSTALLER), Some("x"));
        let params = bug
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or_default();
        for pair in params.split('&') {
            let name = pair.split('=').next().unwrap_or_default();
            if name != "template" {
                assert!(
                    bug_ids.iter().any(|id| id == name),
                    "bug 地址里的参数 {name} 不是模板字段"
                );
            }
        }
        assert!(feature_ids.iter().any(|id| id == "version"));
    }

    #[test]
    fn templates_have_the_labels_titles_and_required_fields_the_card_asks_for() {
        assert!(BUG_TEMPLATE.contains("title: \"[Bug] \""));
        assert!(BUG_TEMPLATE.contains("labels: [\"bug\"]"));
        assert!(BUG_TEMPLATE.contains("render: text"));
        assert!(FEATURE_TEMPLATE.contains("title: \"[Feature] \""));
        assert!(FEATURE_TEMPLATE.contains("labels: [\"enhancement\"]"));
    }

    #[test]
    fn the_buttons_open_urls_for_this_build() {
        let bug = bug_url_for_this_build();
        assert!(
            bug.starts_with(
                "https://github.com/MoeShinX/ZeeTerm/issues/new?template=bug_report.yml"
            ),
            "{bug}"
        );
        assert!(
            bug.contains(&format!("&version={}", percent_encode(&version()))),
            "{bug}"
        );
        assert!(bug.contains("&os="), "{bug}");
        assert!(!bug.contains("logs="), "关于页的按钮不带日志：{bug}");
        let feature = feature_url_for_this_build();
        assert!(
            feature.contains("template=feature_request.yml&version=v"),
            "{feature}"
        );
    }

    #[test]
    fn the_repository_address_is_defined_once() {
        assert!(product::REPO_URL.starts_with("https://github.com/"));
        assert!(!product::REPO_URL.ends_with('/'));
        // 发布页的地址是同一个仓库。
        let releases = crate::about_view::releases_url().expect("内嵌的更新源应能推出发布页");
        assert_eq!(releases, format!("{}/releases", product::REPO_URL));
    }
}
