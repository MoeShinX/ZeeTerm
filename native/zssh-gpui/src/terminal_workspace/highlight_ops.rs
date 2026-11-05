//! 关键词高亮的规则读写与后台扫描结果的收取（从 `terminal_workspace.rs` 拆出来）。

use super::*;

impl TerminalWorkspace {
    // ---- 高亮 ----

    /// 取后台扫描结果；返回是否有新结果。只有和最近一次请求完全对上的结果才会收下
    /// （窗格、连接代次、屏幕内容、规则、视口），迟到的旧结果由调度器丢掉。
    pub(super) fn poll_highlight(&mut self) -> bool {
        let Some(outcome) = self.highlight.scheduler.poll() else {
            return false;
        };
        if !self.settings.highlight_enabled {
            self.highlight.scheduler.forget();
            return false;
        }
        let notice = outcome.notices.first().cloned();
        let error = outcome
            .statuses
            .iter()
            .find_map(|status| status.error.clone())
            .map(|error| format!("规则错误：{error}"));
        self.highlight.status = notice.or(error);
        true
    }

    /// 每帧：看一眼聚焦窗格现在是哪一屏，没请求过就交给后台扫描。
    /// 同一屏（窗格、连接代次、内容修订号、规则、视口都没变）不重复提交。
    pub(super) fn request_highlight_scan(&mut self) {
        self.highlight.current_view = None;
        if !self.settings.highlight_enabled {
            self.highlight.scheduler.forget();
            return;
        }
        if self.highlight.rules.is_empty() {
            return;
        }
        let Some(tab) = self.tabs.active() else {
            return;
        };
        let pane = tab.active();
        let key = pane.scan_key(self.highlight.revision);
        self.highlight.current_view = Some(key);
        let HighlightUi {
            scheduler, rules, ..
        } = &mut self.highlight;
        scheduler.want(key, rules, || {
            pane.snapshot
                .rows
                .iter()
                .map(crate::highlight::line_text)
                .collect()
        });
    }

    /// 直接加一条规则；高亮面板的输入框和验证钩子共用。
    #[cfg(debug_assertions)]
    pub(crate) fn add_highlight_rule(
        &mut self,
        pattern: String,
        case_sensitive: bool,
        cx: &mut Context<Self>,
    ) {
        let id = self.highlight.next_id;
        self.highlight.next_id += 1;
        self.highlight
            .rules
            .push(HighlightRule::new(id, pattern, case_sensitive));
        self.highlight.bump();
        self.persist_highlight_rules(cx);
        cx.notify();
    }

    /// 把这个窗口里改过的规则集写进应用级设置（其他窗口通过订阅换成同一份）。
    pub(super) fn persist_highlight_rules(&mut self, cx: &mut Context<Self>) {
        let rules = self.highlight.rules.clone();
        self.update_settings(cx, move |s| s.highlight_rules = Some(rules));
    }

    pub(crate) fn toggle_highlight_rule(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(rule) = self.highlight.rules.iter_mut().find(|rule| rule.id == id) {
            rule.enabled = !rule.enabled;
            self.highlight.bump();
            self.persist_highlight_rules(cx);
            cx.notify();
        }
    }

    /// 改一条规则（设置页的规则编辑器共用）：改完规则集的修订号加一，后台线程重新编译，并存盘。
    pub(super) fn edit_highlight_rule(
        &mut self,
        id: u64,
        edit: impl FnOnce(&mut HighlightRule),
        cx: &mut Context<Self>,
    ) {
        if let Some(rule) = self.highlight.rules.iter_mut().find(|rule| rule.id == id) {
            edit(rule);
            self.highlight.bump();
            self.persist_highlight_rules(cx);
            cx.notify();
        }
    }

    pub(crate) fn set_highlight_pattern(
        &mut self,
        id: u64,
        pattern: String,
        cx: &mut Context<Self>,
    ) {
        self.edit_highlight_rule(id, |rule| rule.pattern = pattern, cx);
    }

    pub(crate) fn set_highlight_color(&mut self, id: u64, color: u32, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.set_color(color), cx);
    }

    pub(crate) fn set_highlight_regex(&mut self, id: u64, regex: bool, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.regex = regex, cx);
    }

    pub(crate) fn set_highlight_case(
        &mut self,
        id: u64,
        case_sensitive: bool,
        cx: &mut Context<Self>,
    ) {
        self.edit_highlight_rule(id, |rule| rule.case_sensitive = case_sensitive, cx);
    }

    pub(crate) fn set_highlight_background(&mut self, id: u64, on: bool, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.set_background(on), cx);
    }

    /// 添加一条空规则（颜色按现有条数轮换）；返回新规则的编号，设置页据此建它的输入框。
    /// 规则数到上限（和同步校验一致）不再加。空模式的规则不生效，也不报错，等用户填。
    pub(crate) fn add_blank_highlight_rule(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        if self.highlight.rules.len() >= MAX_HIGHLIGHT_RULES {
            return None;
        }
        let id = self.highlight.next_id;
        self.highlight.next_id += 1;
        let mut rule = HighlightRule::new(id, String::new(), false);
        rule.foreground = crate::highlight::HIGHLIGHT_COLORS
            [self.highlight.rules.len() % crate::highlight::HIGHLIGHT_COLORS.len()]
        .0;
        self.highlight.rules.push(rule);
        self.highlight.bump();
        self.persist_highlight_rules(cx);
        cx.notify();
        Some(id)
    }

    /// 补上还没有的常用规则（按模式对比，已有的不重复加）；返回补了几条。
    pub(crate) fn add_sample_highlight_rules(&mut self, cx: &mut Context<Self>) -> usize {
        let missing = crate::highlight::missing_samples(&self.highlight.rules);
        let room = MAX_HIGHLIGHT_RULES.saturating_sub(self.highlight.rules.len());
        let mut added = 0;
        for mut rule in missing.into_iter().take(room) {
            rule.id = self.highlight.next_id;
            self.highlight.next_id += 1;
            self.highlight.rules.push(rule);
            added += 1;
        }
        if added > 0 {
            self.highlight.bump();
            self.persist_highlight_rules(cx);
            cx.notify();
        }
        added
    }

    pub(crate) fn remove_highlight_rule(&mut self, id: u64, cx: &mut Context<Self>) {
        self.highlight.rules.retain(|rule| rule.id != id);
        self.highlight.bump();
        self.persist_highlight_rules(cx);
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub(crate) fn reset_highlight_rules(&mut self, cx: &mut Context<Self>) {
        self.highlight.rules = crate::highlight::default_rules();
        self.highlight.next_id = 8;
        self.highlight.bump();
        self.persist_highlight_rules(cx);
        cx.notify();
    }
}
