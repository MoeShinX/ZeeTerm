//! 主机「属性」对话框的版式（V2-U13）：一个可滚动的单栏表单，从上到下一屏看完。纯逻辑，不依赖 GPUI，
//! 单测直接盯这些规则；界面在 `host_dialog`，表单状态和保存在 `host_panel`，保存规则在 `host_form`。
//!
//! 表单是一串「行」（[`Row`]），按协议显隐。字段一个没加、没删，全是 ZeeTerm 原来表单里的：
//!
//! | 协议 | 行（从上到下） |
//! |---|---|
//! | SSH | 名称 / 协议 / 地址 + 端口 / 认证方式 / 用户名 + 密码（或密钥）/ 分组 / 连接方式 / [代理或跳板机] / 高级设置（折叠：X11 转发、旧算法兼容、主机指纹）/ AI 记忆（折叠，只有已保存的主机有） |
//! | Telnet | 名称 / 协议 / 地址 + 端口 / 用户名 + 密码 / 自动登录 [/ 提示词] / 分组 / 连接方式 / [代理或跳板机] / 高级设置（折叠：回车模式、本地回显、终端类型、编码、NAWS、纯 TCP） |
//! | 串口 | 名称 / 协议 / 设备 + 波特率 / 分组 / 高级设置（折叠：数据位 / 校验 / 停止位、流控、换行、编码、本地回显） |
//!
//! 校验出错时：字段下方红字，自动滚动到出错的那一行；出错字段在折叠区里就先展开它。

use zssh_core::model::Protocol;

use super::host_form::ConnMode;
use super::protocol_form::FormField;

/// 高级设置里的一行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvRow {
    /// SSH：X11 转发。
    X11,
    /// SSH：旧算法兼容。
    Legacy,
    /// SSH：已保存的主机指纹（查看 / 清除）。
    Fingerprint,
    /// Telnet：回车模式 + 本地回显。
    TelnetKeys,
    /// Telnet：终端类型 + 编码。
    TelnetTerm,
    /// Telnet：NAWS。
    TelnetNaws,
    /// Telnet：纯 TCP 模式。
    TelnetRaw,
    /// 串口：数据位 + 校验位 + 停止位。
    SerialFraming,
    /// 串口：流控 + 换行发送模式。
    SerialFlow,
    /// 串口：编码。
    SerialEncoding,
    /// 串口：本地回显。
    SerialEcho,
}

/// 表单里的一行（滚动容器的一个直接子元素，下标就是滚动目标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Name,
    Protocol,
    /// SSH / Telnet：地址 + 端口；串口：设备 + 波特率。
    Endpoint,
    /// SSH：认证方式分段按钮。
    AuthMethod,
    /// 用户名 + 密码（密码认证 / Telnet）或密钥 / ssh-agent 凭据下拉。
    Credentials,
    /// Telnet：自动登录开关卡片。
    AutoLogin,
    /// Telnet：自动登录开着时的用户名 / 密码提示词。
    AutoLoginPrompts,
    Group,
    /// 连接方式分段按钮（SSH / Telnet）。
    Routing,
    /// 连接方式下面的代理 / 跳板机选择（直接连接时没有）。
    RoutingDetail,
    AdvancedHeader,
    Advanced(AdvRow),
    /// AI 记忆折叠区的标题。
    MemoryHeader,
    MemoryBody,
}

impl Row {
    /// 调试脚本里用的短名字。
    pub fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Protocol => "protocol",
            Self::Endpoint => "endpoint",
            Self::AuthMethod => "auth_method",
            Self::Credentials => "credentials",
            Self::AutoLogin => "auto_login",
            Self::AutoLoginPrompts => "auto_login_prompts",
            Self::Group => "group",
            Self::Routing => "routing",
            Self::RoutingDetail => "routing_detail",
            Self::AdvancedHeader => "advanced",
            Self::Advanced(AdvRow::X11) => "x11",
            Self::Advanced(AdvRow::Legacy) => "legacy",
            Self::Advanced(AdvRow::Fingerprint) => "fingerprint",
            Self::Advanced(AdvRow::TelnetKeys) => "telnet_keys",
            Self::Advanced(AdvRow::TelnetTerm) => "telnet_term",
            Self::Advanced(AdvRow::TelnetNaws) => "telnet_naws",
            Self::Advanced(AdvRow::TelnetRaw) => "telnet_raw",
            Self::Advanced(AdvRow::SerialFraming) => "serial_framing",
            Self::Advanced(AdvRow::SerialFlow) => "serial_flow",
            Self::Advanced(AdvRow::SerialEncoding) => "serial_encoding",
            Self::Advanced(AdvRow::SerialEcho) => "serial_echo",
            Self::MemoryHeader => "memory",
            Self::MemoryBody => "memory_body",
        }
    }
}

/// 决定有哪些行的表单状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutState {
    pub protocol: Protocol,
    /// 有没有 AI 记忆这一区（只有已保存的、AI 能操作的 SSH 主机才有）。
    pub has_memory: bool,
    pub advanced_open: bool,
    pub memory_open: bool,
    pub conn_mode: ConnMode,
    /// Telnet：自动登录开着。
    pub auto_login: bool,
}

/// 高级设置展开后的行（按协议）。
pub fn advanced_rows(protocol: Protocol) -> Vec<AdvRow> {
    match protocol {
        Protocol::Ssh => vec![AdvRow::X11, AdvRow::Legacy, AdvRow::Fingerprint],
        Protocol::Telnet => vec![
            AdvRow::TelnetKeys,
            AdvRow::TelnetTerm,
            AdvRow::TelnetNaws,
            AdvRow::TelnetRaw,
        ],
        Protocol::Serial => vec![
            AdvRow::SerialFraming,
            AdvRow::SerialFlow,
            AdvRow::SerialEncoding,
            AdvRow::SerialEcho,
        ],
    }
}

/// 现在表单从上到下有哪些行。
pub fn rows(state: LayoutState) -> Vec<Row> {
    let mut list = vec![Row::Name, Row::Protocol, Row::Endpoint];
    match state.protocol {
        Protocol::Ssh => {
            list.push(Row::AuthMethod);
            list.push(Row::Credentials);
        }
        Protocol::Telnet => {
            list.push(Row::Credentials);
            list.push(Row::AutoLogin);
            if state.auto_login {
                list.push(Row::AutoLoginPrompts);
            }
        }
        Protocol::Serial => {}
    }
    list.push(Row::Group);
    if state.protocol != Protocol::Serial {
        list.push(Row::Routing);
        if state.conn_mode != ConnMode::Direct {
            list.push(Row::RoutingDetail);
        }
    }
    list.push(Row::AdvancedHeader);
    if state.advanced_open {
        list.extend(advanced_rows(state.protocol).into_iter().map(Row::Advanced));
    }
    if state.protocol == Protocol::Ssh && state.has_memory {
        list.push(Row::MemoryHeader);
        if state.memory_open {
            list.push(Row::MemoryBody);
        }
    }
    list
}

/// 一个字段的校验错误显示在哪一行；找不到对应字段的（`General`）显示在底部。
pub fn row_of_field(field: FormField, protocol: Protocol) -> Option<Row> {
    Some(match field {
        FormField::Name => Row::Name,
        FormField::Address | FormField::Port | FormField::Device | FormField::Baud => Row::Endpoint,
        FormField::Username | FormField::Password | FormField::Identity => Row::Credentials,
        FormField::AutoLogin => Row::AutoLogin,
        FormField::Proxy | FormField::Jump => Row::RoutingDetail,
        FormField::X11 => Row::Advanced(AdvRow::X11),
        FormField::TerminalType => Row::Advanced(AdvRow::TelnetTerm),
        // 数据位 / 校验位 / 停止位的组合错误；换了协议它也只属于串口。
        FormField::Framing => {
            let _ = protocol;
            Row::Advanced(AdvRow::SerialFraming)
        }
        FormField::General => return None,
    })
}

/// 这一行在折叠区里吗（出错时要先展开）。
pub fn is_advanced(row: Row) -> bool {
    matches!(row, Row::Advanced(_))
}

/// 该行在 `rows(state)` 里的下标（滚动目标）；这一行现在不存在就是 `None`。
pub fn index_of(state: LayoutState, row: Row) -> Option<usize> {
    rows(state).iter().position(|candidate| *candidate == row)
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
    use super::{
        AdvRow, LayoutState, Row, advanced_rows, dialog_title, index_of, is_advanced, row_of_field,
        rows,
    };
    use crate::main_window::host_form::ConnMode;
    use crate::main_window::protocol_form::FormField;
    use zssh_core::model::Protocol;

    fn state(protocol: Protocol) -> LayoutState {
        LayoutState {
            protocol,
            has_memory: false,
            advanced_open: false,
            memory_open: false,
            conn_mode: ConnMode::Proxy,
            auto_login: false,
        }
    }

    fn ids(state: LayoutState) -> Vec<&'static str> {
        rows(state).into_iter().map(Row::id).collect()
    }

    #[test]
    fn ssh_reads_top_to_bottom_like_the_reference_form() {
        assert_eq!(
            ids(state(Protocol::Ssh)),
            [
                "name",
                "protocol",
                "endpoint",
                "auth_method",
                "credentials",
                "group",
                "routing",
                "routing_detail",
                "advanced"
            ]
        );
    }

    #[test]
    fn telnet_has_no_auth_method_but_keeps_auto_login_under_the_credentials() {
        assert_eq!(
            ids(state(Protocol::Telnet)),
            [
                "name",
                "protocol",
                "endpoint",
                "credentials",
                "auto_login",
                "group",
                "routing",
                "routing_detail",
                "advanced"
            ]
        );
        let on = LayoutState {
            auto_login: true,
            ..state(Protocol::Telnet)
        };
        assert_eq!(
            &ids(on)[3..6],
            ["credentials", "auto_login", "auto_login_prompts"]
        );
    }

    #[test]
    fn serial_has_neither_credentials_nor_routing() {
        assert_eq!(
            ids(state(Protocol::Serial)),
            ["name", "protocol", "endpoint", "group", "advanced"]
        );
    }

    #[test]
    fn direct_connection_has_no_detail_row_and_the_other_two_have_one() {
        for (mode, detail) in [
            (ConnMode::Direct, false),
            (ConnMode::Jump, true),
            (ConnMode::Proxy, true),
        ] {
            let list = rows(LayoutState {
                conn_mode: mode,
                ..state(Protocol::Ssh)
            });
            assert_eq!(list.contains(&Row::RoutingDetail), detail, "{mode:?}");
            assert!(list.contains(&Row::Routing));
        }
    }

    #[test]
    fn the_advanced_section_is_collapsed_by_default_and_holds_each_protocols_leftovers() {
        assert!(!rows(state(Protocol::Ssh)).iter().any(|r| is_advanced(*r)));
        let open = |protocol| LayoutState {
            advanced_open: true,
            ..state(protocol)
        };
        assert_eq!(
            &ids(open(Protocol::Ssh))[9..],
            ["x11", "legacy", "fingerprint"]
        );
        assert_eq!(advanced_rows(Protocol::Telnet).len(), 4);
        assert_eq!(advanced_rows(Protocol::Serial).len(), 4);
        assert!(!advanced_rows(Protocol::Telnet).contains(&AdvRow::X11));
        assert!(!advanced_rows(Protocol::Serial).contains(&AdvRow::Legacy));
    }

    #[test]
    fn the_memory_section_is_a_collapsible_block_after_the_advanced_settings_ssh_only() {
        let saved = LayoutState {
            has_memory: true,
            ..state(Protocol::Ssh)
        };
        let list = ids(saved);
        assert_eq!(
            list.last(),
            Some(&"memory"),
            "新建的主机没有记忆；已保存的才有"
        );
        let open = LayoutState {
            memory_open: true,
            advanced_open: true,
            ..saved
        };
        let list = ids(open);
        assert_eq!(&list[list.len() - 2..], ["memory", "memory_body"]);
        let advanced_at = list
            .iter()
            .position(|r| *r == "advanced")
            .expect("有高级设置");
        let memory_at = list.iter().position(|r| *r == "memory").expect("有记忆");
        assert!(memory_at > advanced_at + 3, "记忆在高级设置之后");
        // 不是 SSH 或没有记忆：没有这一区。
        assert!(
            !ids(LayoutState {
                has_memory: true,
                ..state(Protocol::Telnet)
            })
            .contains(&"memory")
        );
        assert!(!ids(state(Protocol::Ssh)).contains(&"memory"));
    }

    #[test]
    fn every_field_error_lands_on_a_row_the_protocol_can_show_once_the_fold_is_open() {
        use FormField::*;
        let check = |protocol: Protocol, fields: &[FormField]| {
            let open = LayoutState {
                advanced_open: true,
                has_memory: true,
                auto_login: true,
                ..state(protocol)
            };
            for field in fields {
                let row = row_of_field(*field, protocol).expect("有对应的行");
                assert!(
                    index_of(open, row).is_some(),
                    "{protocol:?} 展开后找不到 {field:?} 的行 {row:?}"
                );
            }
        };
        check(
            Protocol::Ssh,
            &[Name, Address, Port, Username, Identity, Proxy, Jump, X11],
        );
        check(
            Protocol::Telnet,
            &[
                Name,
                Address,
                Port,
                Username,
                Password,
                AutoLogin,
                Proxy,
                Jump,
                TerminalType,
            ],
        );
        check(Protocol::Serial, &[Name, Device, Baud, Framing]);
        assert_eq!(
            row_of_field(General, Protocol::Ssh),
            None,
            "找不到字段的错误显示在底部"
        );
    }

    #[test]
    fn errors_inside_the_fold_say_so_and_the_rest_do_not() {
        assert!(is_advanced(
            row_of_field(FormField::X11, Protocol::Ssh).expect("行")
        ));
        assert!(is_advanced(
            row_of_field(FormField::TerminalType, Protocol::Telnet).expect("行")
        ));
        assert!(is_advanced(
            row_of_field(FormField::Framing, Protocol::Serial).expect("行")
        ));
        assert!(!is_advanced(
            row_of_field(FormField::Address, Protocol::Ssh).expect("行")
        ));
        assert!(!is_advanced(
            row_of_field(FormField::Jump, Protocol::Ssh).expect("行")
        ));
    }

    #[test]
    fn the_scroll_target_index_follows_the_open_sections() {
        let closed = state(Protocol::Ssh);
        // 折叠着时高级设置里的行不存在；展开后才有下标（展开让它出现在滚动区里，滚动目标才找得到）。
        assert_eq!(index_of(closed, Row::Advanced(AdvRow::X11)), None);
        let open = LayoutState {
            advanced_open: true,
            ..closed
        };
        assert_eq!(index_of(open, Row::Advanced(AdvRow::X11)), Some(9));
        assert_eq!(index_of(open, Row::Name), Some(0));
        assert_eq!(index_of(closed, Row::Credentials), Some(4));
    }

    #[test]
    fn the_title_names_the_host_or_says_new() {
        assert_eq!(dialog_title(None), "新建主机");
        assert_eq!(dialog_title(Some("web-1")), "web-1 属性");
    }
}
