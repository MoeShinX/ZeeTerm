//! 关闭标签 / 窗格 / 独立终端窗口前的确认（V2-U44 A）：要关什么（[`CloseRequest`]）、范围里有多少正在使用的连接
//! （[`CloseSummary`]）、确认框上写什么（[`CloseSummary::text`]）和确认框的内容卡片（[`dialog_content`]）。
//!
//! 主窗口把它放进自己的 `Modal`，独立终端窗口（没有组件库的 `Root`）放进自己画的应用内确认层，两边的文字和
//! 按钮一模一样。是否要问、范围怎么定都是纯函数，不用起窗口就能测。
//!
//! 范围一律用**标签编号**（`Tab::id`）记，不用下标：确认框开着时标签可能被别处关掉、挪位，确认后只处理当初
//! 选中的那些标签，绝不会因为下标漂移关错别的。

use ::gpui::{AnyElement, App, ClickEvent, SharedString, Window};
use gpui_kit::component::*;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::i18n::{Lang, Msg, current, t};
use crate::main_window::widgets::{ButtonLook, check_card, text_button};
use crate::split_tree::PaneId;
use crate::typography::{Role, Typo as _};

/// 确认框里最多列出几个标签名。
pub const MAX_NAMES: usize = 3;

/// 「关闭标签」的几种范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseScope {
    Others,
    Left,
    Right,
    All,
}

/// 用户这次要关的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseRequest {
    /// 一批标签（按标签条上的顺序）。`anchor` 是用户点的那个标签：活动标签被关掉时，焦点落到它上面
    /// （和关闭其他 / 左侧 / 右侧一直以来的规则一致）；单个标签没有锚点。
    Tabs { ids: Vec<u64>, anchor: Option<u64> },
    /// 一个分屏窗格（所在标签 + 窗格编号）。
    Pane { tab_id: u64, pane_id: PaneId },
}

/// 点了标签条上第 `index` 个标签的「关闭 X」时，要关的标签编号（按标签条顺序）。
/// 纯函数：`ids` 是现在所有标签的编号。越界返回空（什么也不关）。
pub fn scope_ids(ids: &[u64], index: usize, scope: CloseScope) -> Vec<u64> {
    if index >= ids.len() {
        return Vec::new();
    }
    match scope {
        CloseScope::Others => ids
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .map(|(_, id)| *id)
            .collect(),
        CloseScope::Left => ids[..index].to_vec(),
        CloseScope::Right => ids[index + 1..].to_vec(),
        CloseScope::All => ids.to_vec(),
    }
}

/// 一次关闭的范围里，有多少正在使用的连接、涉及哪些标签。由工作区按当时的实际状态算出来。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseSummary {
    /// 范围里状态是「已连上 / 正在连接」的会话窗格个数（离线演示窗格、已断开、空白不算）。
    pub connections: usize,
    /// 范围里一共几个标签（标题用）。
    pub tab_total: usize,
    /// 有正在使用的连接的标签名（按标签条顺序）。
    pub busy_tabs: Vec<String>,
    /// 范围里有 AI 正在用的终端。
    pub ai: bool,
    /// 关的是单个分屏窗格，不是标签。
    pub pane: bool,
}

/// 要不要先问：开关开着，并且范围里至少有一个正在使用的连接。
pub fn needs_confirm(enabled: bool, summary: &CloseSummary) -> bool {
    enabled && summary.connections > 0
}

/// 确认框上的文字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseText {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug)]
enum CloseMsg {
    TitleTabLead,
    TitleTabTail,
    TitleManyLead,
    TitleManyTail,
    TitlePane,
    BodyLead,
    BodyTail,
    /// 「N 个正在使用的连接」里数字后面的量词（英文走单复数，用不到它）。
    ConnectionUnit,
    /// 「N 个标签」里数字后面的量词。
    TabUnit,
    Affected,
    MoreLead,
    QuoteOpen,
    QuoteClose,
    ListSep,
    Colon,
    AiOne,
    AiMany,
    DontAsk,
    DontAskHint,
    Close,
    Cancel,
}

impl Msg for CloseMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TitleTabLead => "关闭",
            Self::TitleTabTail => "？",
            Self::TitleManyLead => "关闭 ",
            Self::TitleManyTail => "？",
            Self::TitlePane => "关闭这个窗格？",
            Self::BodyLead => "这会断开 ",
            Self::BodyTail => "，正在运行的命令会随之结束。",
            Self::ConnectionUnit => " 个正在使用的连接",
            Self::TabUnit => " 个标签",
            Self::Affected => "涉及",
            Self::MoreLead => "等 ",
            Self::QuoteOpen => "「",
            Self::QuoteClose => "」",
            Self::ListSep => "、",
            Self::Colon => "：",
            Self::AiOne => "AI 正在使用这个终端。",
            Self::AiMany => "其中有 AI 正在使用的终端。",
            Self::DontAsk => "不再询问",
            Self::DontAskHint => "以后可以在「设置 → 终端」里重新打开。",
            Self::Close => "关闭",
            Self::Cancel => "取消",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TitleTabLead => "Close ",
            Self::TitleTabTail => "?",
            Self::TitleManyLead => "Close ",
            Self::TitleManyTail => "?",
            Self::TitlePane => "Close this pane?",
            Self::BodyLead => "This will disconnect ",
            Self::BodyTail => ", and any commands running in them will end.",
            Self::ConnectionUnit => "",
            Self::TabUnit => "",
            Self::Affected => "Affected",
            Self::MoreLead => ", ",
            Self::QuoteOpen => "“",
            Self::QuoteClose => "”",
            Self::ListSep => ", ",
            Self::Colon => ": ",
            Self::AiOne => "AI is working in this terminal.",
            Self::AiMany => "AI is working in some of these terminals.",
            Self::DontAsk => "Don't ask again",
            Self::DontAskHint => "You can turn it back on in Settings → Terminal.",
            Self::Close => "Close",
            Self::Cancel => "Cancel",
        }
    }
}

/// 指定语言取文案：文字拼装不碰进程级的当前语言，两种语言都能在单元测试里并行验证。
fn m(lang: Lang, key: CloseMsg) -> &'static str {
    match lang {
        Lang::Zh => key.zh(),
        Lang::En => key.en(),
    }
}

/// 「N 个…」：中文数字 + 量词，英文数字 + 单复数名词。
fn counted(lang: Lang, n: usize, unit: CloseMsg, one: &str, many: &str) -> String {
    match lang {
        Lang::Zh => format!("{n}{}", m(lang, unit)),
        Lang::En => format!("{n} {}", if n == 1 { one } else { many }),
    }
}

fn quoted(lang: Lang, name: &str) -> String {
    format!(
        "{}{name}{}",
        m(lang, CloseMsg::QuoteOpen),
        m(lang, CloseMsg::QuoteClose)
    )
}

impl CloseSummary {
    /// 确认框的标题和正文（当前界面语言）。
    pub fn text(&self) -> CloseText {
        self.text_in(current())
    }

    /// 同 [`Self::text`]，语言由调用方给。标签名是用户自己起的名字，原样显示；数量、连接词走文案表。
    pub fn text_in(&self, lang: Lang) -> CloseText {
        let tabs = |n: usize| counted(lang, n, CloseMsg::TabUnit, "tab", "tabs");
        let title = if self.pane {
            m(lang, CloseMsg::TitlePane).to_owned()
        } else if self.tab_total == 1 && self.busy_tabs.len() == 1 {
            format!(
                "{}{}{}",
                m(lang, CloseMsg::TitleTabLead),
                quoted(lang, &self.busy_tabs[0]),
                m(lang, CloseMsg::TitleTabTail)
            )
        } else {
            format!(
                "{}{}{}",
                m(lang, CloseMsg::TitleManyLead),
                tabs(self.tab_total),
                m(lang, CloseMsg::TitleManyTail)
            )
        };
        let mut body = format!(
            "{}{}{}",
            m(lang, CloseMsg::BodyLead),
            counted(
                lang,
                self.connections,
                CloseMsg::ConnectionUnit,
                "connection in use",
                "connections in use"
            ),
            m(lang, CloseMsg::BodyTail)
        );
        // 关多个标签时列出涉及的标签名（最多 MAX_NAMES 个，多的写「等 N 个标签」）；
        // 单个标签 / 单个窗格标题里已经说清了，不重复。
        if !self.pane && self.tab_total > 1 && !self.busy_tabs.is_empty() {
            let names: Vec<String> = self
                .busy_tabs
                .iter()
                .take(MAX_NAMES)
                .map(|name| quoted(lang, name))
                .collect();
            let mut list = names.join(m(lang, CloseMsg::ListSep));
            if self.busy_tabs.len() > MAX_NAMES {
                list.push_str(m(lang, CloseMsg::MoreLead));
                list.push_str(&tabs(self.busy_tabs.len()));
            }
            body.push('\n');
            body.push_str(m(lang, CloseMsg::Affected));
            body.push_str(m(lang, CloseMsg::Colon));
            body.push_str(&list);
        }
        if self.ai {
            body.push('\n');
            let one = self.pane || self.tab_total <= 1;
            body.push_str(m(
                lang,
                if one {
                    CloseMsg::AiOne
                } else {
                    CloseMsg::AiMany
                },
            ));
        }
        CloseText { title, body }
    }
}

/// 确认框里的按钮 / 勾选框编号（窗口级自动化按它们找）。
pub const ID_OK: &str = "close-confirm-ok";
pub const ID_CANCEL: &str = "close-confirm-cancel";
pub const ID_DONT_ASK: &str = "close-confirm-dont-ask";

/// 确认框的内容卡片（标题、正文、「不再询问」、取消 / 关闭），两个窗口共用。
/// 外面套哪种卡片、怎么收键盘由调用方管（Enter = 关闭，Esc = 取消）。
pub fn dialog_content(
    text: &CloseText,
    dont_ask: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    v_flex()
        .gap_4()
        .child(
            v_flex()
                .gap_2()
                .child(
                    div()
                        .typo(Role::GroupTitle, tokens)
                        .child(SharedString::from(text.title.clone())),
                )
                .child(
                    div()
                        .id("close-confirm-body")
                        .typo(Role::Body, tokens)
                        .child(SharedString::from(text.body.clone())),
                ),
        )
        .child(check_card(
            ID_DONT_ASK,
            t(CloseMsg::DontAsk),
            Some(t(CloseMsg::DontAskHint)),
            dont_ask,
            tokens,
            on_toggle,
        ))
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(text_button(
                    ID_CANCEL,
                    t(CloseMsg::Cancel),
                    ButtonLook::Outline,
                    on_cancel,
                ))
                .child(text_button(
                    ID_OK,
                    t(CloseMsg::Close),
                    ButtonLook::Danger,
                    on_confirm,
                )),
        )
        .into_any_element()
}

/// 确认框上会出现的文字（截图前的隐私检查要把它们算进画面；标签名来自用户）。
#[cfg(debug_assertions)]
pub fn privacy_texts(text: &CloseText) -> Vec<String> {
    vec![text.title.clone(), text.body.clone()]
}

#[cfg(test)]
mod tests {
    // 不用 `use super::*`：父模块 glob 进来的 `gpui_kit::*` 带着它自己的 `test` 宏，会盖掉 `#[test]`。
    use super::{CloseScope, CloseSummary, needs_confirm, scope_ids};
    use crate::i18n::Lang;

    fn summary(connections: usize, tab_total: usize, names: &[&str]) -> CloseSummary {
        CloseSummary {
            connections,
            tab_total,
            busy_tabs: names.iter().map(|name| (*name).to_owned()).collect(),
            ai: false,
            pane: false,
        }
    }

    #[test]
    fn scope_ids_follow_the_clicked_tab() {
        let ids = [10, 11, 12, 13];
        assert_eq!(scope_ids(&ids, 1, CloseScope::Others), [10, 12, 13]);
        assert_eq!(scope_ids(&ids, 2, CloseScope::Left), [10, 11]);
        assert_eq!(scope_ids(&ids, 1, CloseScope::Right), [12, 13]);
        assert_eq!(scope_ids(&ids, 0, CloseScope::All), [10, 11, 12, 13]);
        assert!(scope_ids(&ids, 0, CloseScope::Left).is_empty());
        assert!(scope_ids(&ids, 3, CloseScope::Right).is_empty());
        assert!(
            scope_ids(&ids, 9, CloseScope::All).is_empty(),
            "越界什么也不关"
        );
    }

    #[test]
    fn it_only_asks_when_enabled_and_something_is_in_use() {
        let busy = summary(2, 3, &["a", "b"]);
        assert!(needs_confirm(true, &busy));
        assert!(!needs_confirm(false, &busy), "开关关着不问");
        assert!(
            !needs_confirm(true, &summary(0, 3, &[])),
            "只有断开 / 空白 / 离线的标签直接关"
        );
    }

    #[test]
    fn one_tab_names_itself_in_the_title() {
        let text = summary(1, 1, &["web-1"]).text_in(Lang::Zh);
        assert_eq!(text.title, "关闭「web-1」？");
        assert_eq!(
            text.body,
            "这会断开 1 个正在使用的连接，正在运行的命令会随之结束。"
        );
    }

    #[test]
    fn a_batch_counts_connections_and_lists_at_most_three_names() {
        let text = summary(2, 2, &["web-1", "web-2"]).text_in(Lang::Zh);
        assert_eq!(text.title, "关闭 2 个标签？");
        assert!(
            text.body.contains("断开 2 个正在使用的连接"),
            "{}",
            text.body
        );
        assert!(
            text.body.contains("涉及：「web-1」、「web-2」"),
            "{}",
            text.body
        );
        assert!(!text.body.contains("等 "), "不超过三个不写「等」");

        let many = summary(6, 7, &["a", "b", "c", "d", "e"]).text_in(Lang::Zh);
        assert!(
            many.body.contains("涉及：「a」、「b」、「c」等 5 个标签"),
            "{}",
            many.body
        );
        assert!(!many.body.contains("「d」"), "最多列三个");
        assert_eq!(many.title, "关闭 7 个标签？", "标题数的是范围里所有标签");
    }

    #[test]
    fn a_pane_and_ai_use_have_their_own_wording() {
        let mut pane = summary(1, 1, &["web-1"]);
        pane.pane = true;
        pane.ai = true;
        let text = pane.text_in(Lang::Zh);
        assert_eq!(text.title, "关闭这个窗格？");
        assert!(
            text.body.ends_with("AI 正在使用这个终端。"),
            "{}",
            text.body
        );
        assert!(!text.body.contains("涉及"));

        let mut batch = summary(2, 2, &["a", "b"]);
        batch.ai = true;
        assert!(
            batch
                .text_in(Lang::Zh)
                .body
                .ends_with("其中有 AI 正在使用的终端。"),
            "多个标签用复数说法"
        );
    }

    #[test]
    fn english_wording_uses_plurals_and_quotes() {
        let one = summary(1, 1, &["web-1"]).text_in(Lang::En);
        assert_eq!(one.title, "Close “web-1”?");
        assert_eq!(
            one.body,
            "This will disconnect 1 connection in use, and any commands running in them will end."
        );
        let many = summary(4, 5, &["a", "b", "c", "d"]).text_in(Lang::En);
        assert_eq!(many.title, "Close 5 tabs?");
        assert!(many.body.contains("4 connections in use"), "{}", many.body);
        assert!(
            many.body.contains("Affected: “a”, “b”, “c”, 4 tabs"),
            "{}",
            many.body
        );
    }
}
