//! 调试驱动里 V2-U20（主机的系统图标、主机树虚线连接线）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩两个空函数。
//!
//! - `host_os:<@主机>:<系统 id>`：往库里写这台主机的探测结果（连上过之后真实路径写的就是这张表），
//!   摆不同发行版的图标用；
//! - `assert_host_os:<@主机>:<系统 id|none>`：从库里**重新读**这台主机的探测结果；
//! - `assert_host_icon:<@主机>:<系统 id|none>`：界面现在给这台主机用的图标（`none` = 协议图标）；
//! - `tree_guides:<on|off>`：设置里「主机树显示连接线」开关背后的写入；
//! - `assert_tree_guides:<on|off>`：连接线开关现在的状态（从库里重读）；
//! - `settings_scroll:<像素>`：把设置页内容区向下滚这么多（截图够到页面下半部分的卡用）；
//! - `assert_tree_shapes:<形状,…>`：主机树现在每一行的连接线形状，见 `tree_guides::shape_strings`
//!   （`-` 没线，`T`（├）/ `L`（└）是自己的形状，前面每个祖先层一个 `|`（竖线穿过）或 `.`；脚本里只用 ASCII）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::{host_tree, tree_guides};

    const KNOWN: &[&str] = &[
        "host_os",
        "assert_host_os",
        "assert_host_icon",
        "tree_guides",
        "assert_tree_guides",
        "assert_tree_shapes",
        "settings_scroll",
    ];

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

    fn host_named(this: &MainWindow, raw: &str) -> Result<i64, String> {
        let name = raw.strip_prefix('@').ok_or("主机要写成 @名字")?;
        this.store
            .hosts()
            .iter()
            .find(|host| host.name == name)
            .map(|host| host.id)
            .ok_or_else(|| "没有这个名字的主机".to_owned())
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(match name {
            "host_os" => host_os(this, a, b, cx),
            "assert_host_os" => assert_host_os(this, a, b),
            "assert_host_icon" => assert_host_icon(this, a, b),
            "tree_guides" => tree_guides_switch(this, a, cx),
            "assert_tree_guides" => assert_tree_guides_switch(this, a),
            "settings_scroll" => settings_scroll(this, a, cx),
            _ => assert_tree_shapes(this, a, cx),
        })
    }

    fn host_os(
        this: &mut MainWindow,
        who: &str,
        os: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let id = host_named(this, who)?;
        this.store.set_host_os(id, os)?;
        cx.notify();
        Ok(Outcome::Done)
    }

    fn assert_host_os(this: &MainWindow, who: &str, want: &str) -> Result<Outcome, String> {
        let id = host_named(this, who)?;
        let stored = this
            .store
            .db()?
            .host_os_list()
            .map_err(|err| err.to_string())?;
        let have = stored.get(&id).map_or("none", String::as_str);
        check(have == want, format!("库里是 {have}，不是 {want}"))
    }

    fn assert_host_icon(this: &MainWindow, who: &str, want: &str) -> Result<Outcome, String> {
        let id = host_named(this, who)?;
        let host = this.store.host(id).ok_or("主机不在列表里")?;
        let have = this.store.os_icon_of(host).unwrap_or("none");
        check(have == want, format!("界面用的是 {have}，不是 {want}"))
    }

    fn settings_scroll(
        this: &mut MainWindow,
        pixels: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let pixels: f32 = pixels.parse().map_err(|_| "像素要是数字")?;
        this.settings_scroll
            .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(-pixels)));
        cx.notify();
        Ok(Outcome::Done)
    }

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
    }

    fn tree_guides_switch(
        this: &mut MainWindow,
        raw: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        this.store.set_tree_guides(on_off(raw)?)?;
        cx.notify();
        Ok(Outcome::Done)
    }

    fn assert_tree_guides_switch(this: &MainWindow, raw: &str) -> Result<Outcome, String> {
        let want = on_off(raw)?;
        // 重新读库，不看内存里的缓存。
        let stored = this
            .store
            .db()?
            .setting_get(super::super::host_store::TREE_GUIDES_KEY)
            .map_err(|err| err.to_string())?;
        let have = super::super::bar_settings::parse_bool(stored.as_deref(), true);
        check(
            have == want && this.store.tree_guides() == want,
            format!("库里是 {have}，界面是 {}", this.store.tree_guides()),
        )
    }

    fn assert_tree_shapes(
        this: &MainWindow,
        want: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let query = this.tree_query.read(cx).value().to_string();
        let filter = host_tree::tree_filter(
            this.store.groups(),
            this.store.hosts(),
            &query,
            this.store.flat(),
        );
        let rows = host_tree::visible_rows(
            this.store.groups(),
            this.store.hosts(),
            filter.as_ref(),
            &this.tree.expanded,
            this.store.flat(),
        );
        // 脚本里只用 ASCII：├ 写成 T，└ 写成 L。
        let have = tree_guides::shape_strings(&rows)
            .join(",")
            .replace('├', "T")
            .replace('└', "L");
        check(have == want, format!("现在是 {have}，不是 {want}"))
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
