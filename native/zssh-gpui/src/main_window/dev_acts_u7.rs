//! 调试驱动里 V2-U7（主机属性对话框）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//!
//! - `assert_u7:categories:<id,id,...>`：左侧现在列的类别（`general` `auth` `connection` `terminal` `serial`
//!   `security` `memory`，从上到下）就是这一串；
//! - `assert_u7:title:<文字>`：对话框标题；
//! - `assert_u7:overlay:none|new_proxy|discard`：对话框上叠着哪个小浮层；
//! - `assert_u7:error_dot:<类别>:on|off`：左侧这个类别有没有标红点（里面有字段挂着校验错误）；
//! - `host_new_proxy:open`、`host_new_proxy:set:<name|host|port|username|secret>:<值>`、`host_new_proxy:save`：
//!   叠在对话框上的「新建代理」小对话框（按钮用真点击，这里只负责填字）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::host_dialog::Overlay;
    use super::super::host_props::{self, Category};
    use super::super::modal::Modal;

    const KNOWN: &[&str] = &["assert_u7", "host_new_proxy"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn check(ok: bool, what: String) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            Err(format!("断言没有成立：{what}"))
        }
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(4, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        Some(match name {
            "assert_u7" => assert_u7(this, a, b, c),
            _ => new_proxy(this, a, b, c, window, cx),
        })
    }

    fn assert_u7(this: &MainWindow, a: &str, b: &str, c: &str) -> Result<Outcome, String> {
        // 参数里可能有 `:`（标题不会有，类别和值没有），b、c 合起来还原。
        let rest = if c.is_empty() {
            b.to_owned()
        } else {
            format!("{b}:{c}")
        };
        let form = this.host_form.as_ref().ok_or("主机属性没开着")?;
        match a {
            "categories" => {
                let have: Vec<&str> = form.categories().into_iter().map(Category::id).collect();
                check(
                    have.join(",") == rest,
                    format!("类别是 {}，不是 {rest}", have.join(",")),
                )
            }
            "title" => {
                let title =
                    host_props::dialog_title(form.existing.as_ref().map(|host| host.name.as_str()));
                check(title == rest, format!("标题是 {title}，不是 {rest}"))
            }
            "overlay" => {
                let Some(Modal::HostProps(dialog)) = &this.modal else {
                    return Err("属性对话框没开着".to_owned());
                };
                let have = match dialog.overlay {
                    Overlay::None => "none",
                    Overlay::NewProxy(_) => "new_proxy",
                    Overlay::Discard => "discard",
                };
                check(have == rest, format!("小浮层是 {have}，不是 {rest}"))
            }
            "error_dot" => {
                let category = Category::parse(b).ok_or("不认识的类别")?;
                let on = form
                    .errors
                    .keys()
                    .any(|field| host_props::category_of(*field) == Some(category));
                check(
                    on == (c == "on"),
                    format!("类别 {b} 的红点是 {on}，不是 {c}"),
                )
            }
            _ => Err("assert_u7:categories|title|overlay|error_dot:<值>".to_owned()),
        }
    }

    fn new_proxy(
        this: &mut MainWindow,
        a: &str,
        b: &str,
        c: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match a {
            "open" => this.host_open_new_proxy(window, cx),
            "save" => this.host_save_new_proxy(window, cx),
            "set" => {
                let Some(Modal::HostProps(dialog)) = &this.modal else {
                    return Err("属性对话框没开着".to_owned());
                };
                let Overlay::NewProxy(form) = &dialog.overlay else {
                    return Err("新建代理的小对话框没开着".to_owned());
                };
                let state = match b {
                    "name" => form.name.clone(),
                    "host" => form.host.clone(),
                    "port" => form.port.clone(),
                    "username" => form.username.clone(),
                    "secret" => form.secret.clone(),
                    _ => return Err("字段写 name / host / port / username / secret".to_owned()),
                };
                let value = c.to_owned();
                state.update(cx, |input, cx| input.set_value(value, window, cx));
            }
            _ => return Err("host_new_proxy:open|set:<字段>:<值>|save".to_owned()),
        }
        Ok(Outcome::Done)
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
