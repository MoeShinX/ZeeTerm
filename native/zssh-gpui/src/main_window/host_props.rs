//! 主机「属性」对话框的纯逻辑（V2-U7）：左侧有哪些类别（随协议变）、保存校验的错误该跳到哪个类别、
//! 对话框的标题。不依赖 GPUI，单测直接盯这些规则；界面在 `host_dialog`，表单状态和保存在 `host_panel`。
//!
//! 类别是把原来「基本 / 连接 / 认证 / 网络 / 高级设置（折叠）/ AI 记忆」几块平铺的表单按用途重新分了组，
//! 字段一个没加、没删：
//!
//! | 协议 | 类别 |
//! |---|---|
//! | SSH | 常规 / 认证 / 连接 / 安全 / AI 记忆（只有已保存的主机有） |
//! | Telnet | 常规 / 认证与自动登录 / 连接 / 终端 |
//! | 串口 | 常规 / 串口参数 |

use zssh_core::model::Protocol;

use super::protocol_form::FormField;

/// 对话框左侧的一个类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    /// 名称、协议、地址 / 端口（串口是设备 / 波特率）、分组。
    General,
    /// 用户名、认证方式、密码（密码认证）/ 密钥下拉（私钥认证）；Telnet 还有自动登录。
    Auth,
    /// 代理、跳板机；SSH 还有 X11 转发。
    Connection,
    /// Telnet 的回车模式、本地回显、NAWS、终端类型、编码、纯 TCP。
    Terminal,
    /// 串口的数据位 / 校验位 / 停止位、流控、换行、本地回显、编码。
    SerialParams,
    /// SSH 的旧算法兼容、已保存的主机指纹。
    Security,
    /// SSH 的 AI 记忆。
    Memory,
}

impl Category {
    /// 左侧列表里显示的名字（Telnet 的认证类别里还有自动登录，名字带上它）。
    pub fn label(self, protocol: Protocol) -> &'static str {
        match self {
            Self::General => "常规",
            Self::Auth if protocol == Protocol::Telnet => "认证与自动登录",
            Self::Auth => "认证",
            Self::Connection => "连接",
            Self::Terminal => "终端",
            Self::SerialParams => "串口参数",
            Self::Security => "安全",
            Self::Memory => "AI 记忆",
        }
    }

    /// 调试脚本里用的短名字。
    pub fn id(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Auth => "auth",
            Self::Connection => "connection",
            Self::Terminal => "terminal",
            Self::SerialParams => "serial",
            Self::Security => "security",
            Self::Memory => "memory",
        }
    }

    #[cfg(debug_assertions)]
    pub fn parse(id: &str) -> Option<Self> {
        [
            Self::General,
            Self::Auth,
            Self::Connection,
            Self::Terminal,
            Self::SerialParams,
            Self::Security,
            Self::Memory,
        ]
        .into_iter()
        .find(|category| category.id() == id)
    }
}

/// 这个协议下对话框左侧列哪些类别（从上到下）。`has_memory`：有没有 AI 记忆小节
/// （只有已保存的、AI 能操作的 SSH 主机才有）。
pub fn categories(protocol: Protocol, has_memory: bool) -> Vec<Category> {
    match protocol {
        Protocol::Ssh => {
            let mut list = vec![
                Category::General,
                Category::Auth,
                Category::Connection,
                Category::Security,
            ];
            if has_memory {
                list.push(Category::Memory);
            }
            list
        }
        Protocol::Telnet => vec![
            Category::General,
            Category::Auth,
            Category::Connection,
            Category::Terminal,
        ],
        Protocol::Serial => vec![Category::General, Category::SerialParams],
    }
}

/// 切协议之后，原来停着的类别可能不存在了（比如从 SSH 的「安全」切到 Telnet）：回到「常规」。
pub fn fit(current: Category, protocol: Protocol, has_memory: bool) -> Category {
    if categories(protocol, has_memory).contains(&current) {
        current
    } else {
        Category::General
    }
}

/// 一个字段的校验错误显示在哪个类别里。`General` 错误（找不到对应字段的）不属于任何类别，
/// 显示在对话框底部。
pub fn category_of(field: FormField) -> Option<Category> {
    Some(match field {
        FormField::Name | FormField::Address | FormField::Port => Category::General,
        // 串口的设备 / 波特率和名称一起在「常规」里，数据位 / 停止位在「串口参数」里。
        FormField::Device | FormField::Baud => Category::General,
        FormField::Framing => Category::SerialParams,
        FormField::Username | FormField::Password | FormField::Identity | FormField::AutoLogin => {
            Category::Auth
        }
        FormField::Proxy | FormField::Jump | FormField::X11 => Category::Connection,
        FormField::TerminalType => Category::Terminal,
        FormField::General => return None,
    })
}

/// 对话框标题：新建是「新建主机」，编辑是「<主机名> 属性」。
pub fn dialog_title(existing_name: Option<&str>) -> String {
    match existing_name {
        Some(name) => format!("{name} 属性"),
        None => "新建主机".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{Category, categories, category_of, dialog_title, fit};
    use crate::main_window::protocol_form::FormField;
    use zssh_core::model::Protocol;

    fn labels(protocol: Protocol, memory: bool) -> Vec<&'static str> {
        categories(protocol, memory)
            .into_iter()
            .map(|category| category.label(protocol))
            .collect()
    }

    #[test]
    fn every_protocol_gets_its_own_category_list() {
        assert_eq!(
            labels(Protocol::Ssh, true),
            ["常规", "认证", "连接", "安全", "AI 记忆"]
        );
        assert_eq!(
            labels(Protocol::Ssh, false),
            ["常规", "认证", "连接", "安全"],
            "新建的主机还没有记忆可看"
        );
        assert_eq!(
            labels(Protocol::Telnet, true),
            ["常规", "认证与自动登录", "连接", "终端"],
            "Telnet 没有 AI 记忆，也没有 SSH 才有的安全类别"
        );
        assert_eq!(labels(Protocol::Serial, true), ["常规", "串口参数"]);
    }

    #[test]
    fn a_category_that_disappears_on_a_protocol_switch_falls_back_to_general() {
        assert_eq!(
            fit(Category::Security, Protocol::Telnet, false),
            Category::General
        );
        assert_eq!(
            fit(Category::Terminal, Protocol::Ssh, true),
            Category::General
        );
        assert_eq!(
            fit(Category::Auth, Protocol::Telnet, false),
            Category::Auth,
            "两边都有的类别原地不动"
        );
        assert_eq!(
            fit(Category::Memory, Protocol::Ssh, false),
            Category::General
        );
    }

    #[test]
    fn a_validation_error_jumps_to_the_category_that_shows_its_field() {
        use FormField::*;
        for (field, category) in [
            (Name, Category::General),
            (Address, Category::General),
            (Port, Category::General),
            (Device, Category::General),
            (Baud, Category::General),
            (Framing, Category::SerialParams),
            (Username, Category::Auth),
            (Password, Category::Auth),
            (Identity, Category::Auth),
            (AutoLogin, Category::Auth),
            (Proxy, Category::Connection),
            (Jump, Category::Connection),
            (X11, Category::Connection),
            (TerminalType, Category::Terminal),
        ] {
            assert_eq!(category_of(field), Some(category), "{field:?}");
        }
        assert_eq!(
            category_of(General),
            None,
            "找不到字段的错误显示在对话框底部"
        );
    }

    #[test]
    fn every_field_error_lands_on_a_category_the_protocol_actually_lists() {
        // 某个协议下会报错的字段，它的类别在这个协议的列表里：不会跳到一个看不见的类别。
        let reachable = |protocol: Protocol, fields: &[FormField]| {
            let list = categories(protocol, true);
            for field in fields {
                let category = category_of(*field).expect("有类别");
                assert!(list.contains(&category), "{protocol:?} 里没有 {category:?}");
            }
        };
        use FormField::*;
        reachable(
            Protocol::Ssh,
            &[Name, Address, Port, Username, Identity, Proxy, Jump, X11],
        );
        reachable(
            Protocol::Telnet,
            &[
                Name,
                Address,
                Port,
                Username,
                Identity,
                AutoLogin,
                Proxy,
                Jump,
                TerminalType,
            ],
        );
        reachable(Protocol::Serial, &[Name, Device, Baud, Framing]);
    }

    #[test]
    fn the_title_names_the_host_or_says_new() {
        assert_eq!(dialog_title(None), "新建主机");
        assert_eq!(dialog_title(Some("web-1")), "web-1 属性");
    }
}
