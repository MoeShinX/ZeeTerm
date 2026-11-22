//! 调试驱动里 V2-U34（侧栏属性面板：值可选中复制、密码行）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里这里只剩两个空函数。动作走和界面同一批方法，不会绕过真实的取值 / 丢弃逻辑。
//!
//! - `info_eye`：点属性面板里密码行的小眼睛（显示 ⇄ 隐藏）；
//! - `assert_info_pw:<hidden|shown>`：密码是不是隐藏（隐藏 = 内存里没有明文）；条件暂时不成立会重试到步骤超时，
//!   所以「切主机 / 收起 / 锁屏以后恢复隐藏」这类要等下一帧才发生的事也能直接断言；
//! - `assert_info_pw_text:<文字>`：显示着的明文正好是这段文字；
//! - `assert_info_rows:<行 key，逗号分隔>`：面板现在有哪几行（`password` 是密码行）；
//! - `assert_selection:<文字>`：窗口里现在高亮选中的文字（Ctrl+C 会复制的那份，读它不碰剪贴板）正好是这段文字；
//!   `assert_selection:none` 是没有选区；
//! - `info_copy:<行 key>`：就是右键「复制」会做的事（普通行复制整行的值，密码行只有显示着才复制）；
//!   复制内容用 `ai_clip` 断言（先 `ai_clip:clear`，抓取模式下不碰系统剪贴板）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::page_logic::{host_info_rows, info_row_copy_text};

    const KNOWN: &[&str] = &[
        "info_eye",
        "assert_info_pw",
        "assert_info_pw_text",
        "assert_info_rows",
        "info_copy",
        "assert_selection",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
        if !is_known(name) {
            return None;
        }
        Some(act(this, name, arg, window, cx))
    }

    fn act(
        this: &mut MainWindow,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        if name == "assert_selection" {
            let have = gpui_kit::base::TextSelection::selected_text(window, cx);
            let ok = if arg == "none" {
                have.is_empty()
            } else {
                have == arg
            };
            return Ok(if ok { Outcome::Done } else { Outcome::Pending });
        }
        // 面板收起后（没有主机）明文也必须没了，所以「隐藏」的断言不需要面板还开着。
        if name == "assert_info_pw" && arg == "hidden" {
            return Ok(if this.info_secret.is_none() {
                Outcome::Done
            } else {
                Outcome::Pending
            });
        }
        let host_id = this.info_host.ok_or("属性面板没有显示任何主机")?;
        let host = this
            .store
            .host(host_id)
            .cloned()
            .ok_or("属性面板的主机不存在")?;
        let rows = host_info_rows(
            &host,
            this.store.hosts(),
            this.store.proxies(),
            this.store.identities(),
        );
        let wait = |ok: bool| {
            if ok {
                Ok(Outcome::Done)
            } else {
                Ok(Outcome::Pending)
            }
        };
        match name {
            "info_eye" => {
                this.toggle_info_password(host_id, window, cx);
                Ok(Outcome::Done)
            }
            "assert_info_pw" => {
                let shown = this.info_secret.is_some();
                match arg {
                    "hidden" => wait(!shown),
                    "shown" => wait(this.info_revealed(host_id).is_some()),
                    _ => Err("assert_info_pw:<hidden|shown>".to_owned()),
                }
            }
            "assert_info_pw_text" => wait(this.info_revealed(host_id) == Some(arg)),
            "assert_info_rows" => {
                let keys: Vec<&str> = rows.iter().map(|row| row.key).collect();
                wait(keys.join(",") == arg)
            }
            _ => {
                let row = rows
                    .iter()
                    .find(|row| row.key == arg)
                    .ok_or_else(|| format!("面板里没有 {arg} 这一行"))?;
                if arg == "password" {
                    this.copy_info_password(host_id, window, cx);
                } else if let Some(text) = info_row_copy_text(row, this.info_revealed(host_id)) {
                    this.copy_info_value(text, window, cx);
                }
                Ok(Outcome::Done)
            }
        }
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
