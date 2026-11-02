//! 产品身份：面向用户的名字，以及试用版的更新开关。
//!
//! ZeeTerm（Z-SSH 的 GPUI 原生版）以独立身份存在，和 Z-SSH（Tauri 版）并存：产品名、安装目录、卸载键、主程序名、
//! 默认数据目录都与 Tauri 版不同（安装包的对应部分见 `native/installer/zssh-native.nsi`，
//! 两边的名字有测试互相核对）。内部的 crate 名、模块名、`ZSSH_*` 环境变量都不跟着改。
//!
//! 以后要接管 Tauri 版的身份时，要改的东西集中在这里和安装包脚本，清单见 `docs/reports/V2-G12.md`。

/// 面向用户的产品名：窗口标题、托盘、关于页、对话框标题、安装包都用它。
pub const NAME: &str = "ZeeTerm";

/// 默认数据目录是 `%APPDATA%\<这个名字>`（`ZSSH_DATA_DIR` 优先）。
pub const DATA_DIR_NAME: &str = "ZeeTerm";

/// 自动更新开关（试用版：关）。
///
/// 为什么关：更新器的公钥和清单地址取自 Tauri 版的配置，会把 Tauri 版的安装包当成"新版本"，
/// 而两个产品的身份不同，装上去会得到第二个产品或覆盖错对象。关闭时不做任何到正式更新源的
/// 请求：不自动检查、不自动下载、退出时不安装，手动"检查更新"只显示说明并引导去发布页。
/// 接管身份时把它改成 `true`，并按报告里的清单补上自动检查与偏好；验签、版本绑定、下载加固
/// 这些 F2 的代码一直保留在 `update_demo` / `zssh-update` 里。
pub const AUTO_UPDATE_ENABLED: bool = false;

/// 试用版点"检查更新"时显示的说明（措辞参照 0.2.14 免安装版的提示）。
pub const TRIAL_UPDATE_NOTICE: &str = "试用版不能自动更新：请到发布页下载新的安装包，直接运行即可覆盖安装（数据在数据目录里，不受影响）";

/// 更新一行默认的说明（还没点过"检查更新"）。
pub const TRIAL_UPDATE_HINT: &str = "试用版不会自动检查或下载更新；新版本请到发布页下载。";

/// "启动时自动检查"在试用版里的说明。
pub const TRIAL_AUTO_CHECK_NOTE: &str = "试用版没有自动更新，这个开关暂不可用。";

/// 仓库根的第三方声明原文（和 Tauri 版共用同一份文件）。
const NOTICE_SOURCE: &str = include_str!("../../../NOTICE");

/// ZeeTerm 额外使用的第三方组件（GPUI、Lucide 图标、语法高亮等）。这些只在原生版里分发，
/// Tauri 版并不带，所以不能写进共用的根 NOTICE（Tauri 版关于页内置的副本要和根 NOTICE 完全一致），
/// 单独放在 `native/NOTICE`。
const NATIVE_NOTICE_SOURCE: &str = include_str!("../../NOTICE");

/// 把共用文件里写的产品名换成本产品的名字。NOTICE / LICENSE 是仓库根的共用文件（Tauri 版也在用），
/// 所以不改文件本身，在界面显示和打安装包时各替换一次；第三方署名里没有这个词，原样保留。
pub fn brand_text(text: &str) -> String {
    text.replace("Z-SSH", NAME)
}

/// 完整声明 = 根 NOTICE + 空一行 + `native/NOTICE`，换行统一成 `\n`（Windows 检出可能是 CRLF）。
/// `native/installer/build.ps1` 打安装包时用同一个拼法（有测试核对），两边内容一致。
fn combine_notices(root: &str, native: &str) -> String {
    let root = root.replace("\r\n", "\n");
    let native = native.replace("\r\n", "\n");
    format!("{}\n\n{}", root.trim_end(), native)
}

/// 关于页"开源许可"里显示的第三方声明：根 NOTICE 与 `native/NOTICE` 合并，再把产品名换成 ZeeTerm。
pub fn notice_text() -> String {
    brand_text(&combine_notices(NOTICE_SOURCE, NATIVE_NOTICE_SOURCE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trial_build_has_its_own_names() {
        assert_eq!(NAME, "ZeeTerm");
        assert_eq!(DATA_DIR_NAME, "ZeeTerm");
        // 不能和 Tauri 版重名：并存的前提。
        assert_ne!(NAME, "Z-SSH");
    }

    #[test]
    fn the_notice_is_branded_and_keeps_third_party_credits() {
        let notice = notice_text();
        assert!(notice.starts_with("ZeeTerm\n") || notice.starts_with("ZeeTerm\r\n"));
        assert!(!notice.contains("Z-SSH"), "产品名都换掉");
        assert!(notice.contains("Netcatty"), "第三方署名不动");
        assert!(notice.contains("Apache-2.0"));
        assert_eq!(brand_text("Z-SSH 和 zssh-core"), "ZeeTerm 和 zssh-core");
    }

    /// 去掉空行、行尾空白后的行集合：用来核对"合并结果没丢任何一条"。
    fn content_lines(text: &str) -> std::collections::BTreeSet<String> {
        text.lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_merged_notice_has_every_line_of_both_files() {
        let merged = notice_text();
        let merged_lines = content_lines(&merged);
        for (name, source) in [
            ("根 NOTICE", NOTICE_SOURCE),
            ("native/NOTICE", NATIVE_NOTICE_SOURCE),
        ] {
            for line in content_lines(&brand_text(source)) {
                assert!(
                    merged_lines.contains(&line),
                    "{name} 的这一行没进合并结果：{line}"
                );
            }
        }
        // 两份各有自己的条目，根 NOTICE 在前。
        let netcatty = merged.find("Netcatty").expect("根 NOTICE 的条目");
        let gpui = merged
            .find("GPUI, gpui-component")
            .expect("native/NOTICE 的条目");
        assert!(netcatty < gpui);
        assert!(merged.contains("Lucide"));
        assert!(merged.contains("syntect"));
    }

    #[test]
    fn native_only_components_live_in_native_notice_not_the_shared_root() {
        // Tauri 版不分发这些组件；根 NOTICE 要和 ui/src/assets/NOTICE.txt 完全一致。
        for native_only in [
            "GPUI",
            "gpui-component",
            "Lucide",
            "syntect",
            "alacritty_terminal",
        ] {
            assert!(
                !NOTICE_SOURCE.contains(native_only),
                "根 NOTICE 不该有 {native_only}"
            );
            assert!(
                NATIVE_NOTICE_SOURCE.contains(native_only),
                "native/NOTICE 缺 {native_only}"
            );
        }
    }

    #[test]
    fn combining_uses_one_blank_line_and_normalizes_line_endings() {
        assert_eq!(combine_notices("a\n", "b\n"), "a\n\nb\n");
        assert_eq!(combine_notices("a\r\n\r\n", "b\r\n"), "a\n\nb\n");
    }
    #[test]
    fn auto_update_stays_off_until_the_identity_is_taken_over() {
        // 这条测试故意写死：谁把开关改成 true，就得同时看一遍报告里的接管清单。
        const {
            assert!(
                !AUTO_UPDATE_ENABLED,
                "试用版不能连正式更新源；接管身份时再改这里和这条测试"
            );
        }
    }
}
