//! Windows 免安装版（zip 解压即用）：程序旁边有一个叫 `portable` 的标记文件，就是免安装版。
//!
//! 和安装版的区别只有这几处，其余行为一样：
//! - 数据目录是 `<程序所在目录>\data`，不碰 `%APPDATA%`（`ZSSH_DATA_DIR` 仍然优先，见 `settings::data_dir`）；
//! - 单实例的互斥体名、窗口属性名带上数据目录路径的短哈希：免安装版和安装版、两份不同目录的免安装版可以同时开，
//!   同一个目录再开一份还是会被拦下；安装版的名字不变（老版本升级上来仍是同一个名字）；
//! - 不写注册表（没有卸载项；程序本来也没有开机自启功能）；
//! - 自动更新走 `latest.json` 里的 `windows-x86_64-portable`，原地替换程序文件（`zssh_update::portable`）。
//!
//! 判断"是不是免安装版"的逻辑在 `zssh_update::portable`（纯函数，有测试）；这里只负责把它接到当前进程上。
//! 其他平台恒为"不是免安装版"：Linux 有 tar.gz，macOS 是 .app。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use zssh_update::portable as core;
use zssh_update::portable::PortableLayout;

/// 免安装版的程序文件；不是免安装版（或读不到自己的路径）时为 `None`。进程里只判断一次。
fn portable_exe() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| {
        if !cfg!(windows) {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        core::has_marker(dir).then_some(exe)
    })
    .as_deref()
}

/// 本进程是不是免安装版。
pub(crate) fn is_portable() -> bool {
    portable_exe().is_some()
}

/// 免安装版的默认数据目录（`<程序所在目录>\data`）；安装版返回 `None`。只有 Windows 有免安装版。
#[cfg(windows)]
pub(crate) fn data_dir() -> Option<PathBuf> {
    let exe = portable_exe()?;
    core::data_dir_for(exe.parent()?, true)
}

/// 免安装版的程序布局（正在运行的程序和它所在的目录）；安装版返回 `None`。
pub(crate) fn layout() -> Option<PortableLayout> {
    portable_exe().map(PortableLayout::new)
}

/// 单实例用的名字：互斥体名和窗口属性名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstanceNames {
    pub mutex: String,
    pub window_prop: String,
}

/// 安装版（`portable_data_dir` 为 `None`）直接用基础名字，一个字不变；免安装版在后面加上数据目录的短哈希。
pub(crate) fn instance_names(
    mutex: &str,
    window_prop: &str,
    portable_data_dir: Option<&Path>,
) -> InstanceNames {
    match portable_data_dir {
        None => InstanceNames {
            mutex: mutex.to_owned(),
            window_prop: window_prop.to_owned(),
        },
        Some(dir) => {
            let hash = core::instance_hash(dir);
            InstanceNames {
                mutex: format!("{mutex}-{hash}"),
                window_prop: format!("{window_prop}-{hash}"),
            }
        }
    }
}

/// 关于页版本号旁边的标注；安装版没有。
pub(crate) fn about_tag() -> Option<&'static str> {
    is_portable().then(|| crate::i18n::t(PortableMsg::Tag))
}

/// 免安装版的界面文案。
#[derive(Clone, Copy, Debug)]
pub(crate) enum PortableMsg {
    /// 关于页版本号旁边的标注。
    Tag,
    /// 数据目录下面的提醒：已保存的密码绑定当前 Windows 用户，换电脑要走离线备份文件。
    PasswordNote,
}

impl crate::i18n::Msg for PortableMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Tag => "免安装版",
            Self::PasswordNote => {
                "已保存的密码用 Windows 用户账户加密（DPAPI），只能在当前电脑、当前 Windows 用户下解开。把免安装版拷到别的电脑或别的 Windows 用户下，已保存的密码会解不开；换电脑请先到「备份」页的「离线备份文件」导出（用密码加密），再到新电脑上导入。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Tag => "Portable",
            Self::PasswordNote => {
                "Saved passwords are encrypted with your Windows account (DPAPI) and can only be decrypted on this computer under this Windows user. If you copy the portable version to another computer or another Windows user, the saved passwords cannot be decrypted; to move to a new computer, export an \"Offline Backup File\" (password-encrypted) from the Backup page first, then import it on the new computer."
            }
        }
    }
}

/// 数据目录下面的密码提醒；安装版没有。
pub(crate) fn password_note() -> Option<&'static str> {
    is_portable().then(|| crate::i18n::t(PortableMsg::PasswordNote))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Msg as _;

    #[test]
    fn the_installed_version_keeps_its_instance_names_unchanged() {
        let names = instance_names("ZeeTerm-instance", "ZeeTerm-main", None);
        assert_eq!(names.mutex, "ZeeTerm-instance");
        assert_eq!(names.window_prop, "ZeeTerm-main");
    }

    #[test]
    fn a_portable_copy_gets_its_own_names_per_data_directory() {
        let a = instance_names(
            "ZeeTerm-instance",
            "ZeeTerm-main",
            Some(Path::new(r"D:\Tools\ZeeTerm\data")),
        );
        let b = instance_names(
            "ZeeTerm-instance",
            "ZeeTerm-main",
            Some(Path::new(r"E:\Other\ZeeTerm\data")),
        );
        // 带哈希、不等于安装版的名字（可以同时开），两份不同目录也不一样。
        assert!(a.mutex.starts_with("ZeeTerm-instance-"));
        assert!(a.window_prop.starts_with("ZeeTerm-main-"));
        assert_ne!(a.mutex, "ZeeTerm-instance");
        assert_ne!(a, b);
        // 同一个目录的不同写法是同一个实例（再开一份要被拦下）。
        let same = instance_names(
            "ZeeTerm-instance",
            "ZeeTerm-main",
            Some(Path::new(r"d:/tools/zeeterm/DATA/")),
        );
        assert_eq!(a, same);
        // 互斥体名和窗口属性名用同一个哈希，后启动的实例才找得到对应的窗口。
        assert_eq!(
            a.mutex.trim_start_matches("ZeeTerm-instance-"),
            a.window_prop.trim_start_matches("ZeeTerm-main-")
        );
    }

    #[test]
    fn the_test_binary_is_not_a_portable_copy() {
        // 测试程序旁边没有 `portable` 标记：别的用例读的数据目录不能被它改变。
        assert!(!is_portable());
        #[cfg(windows)]
        assert_eq!(data_dir(), None);
        assert_eq!(layout(), None);
        assert_eq!(about_tag(), None);
        assert_eq!(password_note(), None);
    }

    #[test]
    fn the_portable_texts_exist_in_both_languages_and_warn_about_moving_computers() {
        for msg in [PortableMsg::Tag, PortableMsg::PasswordNote] {
            assert!(!msg.zh().is_empty() && !msg.en().is_empty());
        }
        assert_eq!(PortableMsg::Tag.zh(), "免安装版");
        assert_eq!(PortableMsg::Tag.en(), "Portable");
        // 密码说明要同时交代清楚：绑定当前用户、拷走会解不开、换电脑的做法。
        let zh = PortableMsg::PasswordNote.zh();
        assert!(zh.contains("DPAPI") && zh.contains("别的电脑") && zh.contains("离线备份文件"));
        let en = PortableMsg::PasswordNote.en();
        assert!(en.contains("DPAPI") && en.contains("another computer"));
        assert!(en.contains("Offline Backup File"));
    }
}
