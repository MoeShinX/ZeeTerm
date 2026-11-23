//! 崩溃后的反馈提示（V2-U62）：上次是 panic 退出的，启动后问用户要不要把崩溃记录反馈给开发者。
//!
//! 判断「有没有新记录」和拼地址的逻辑在 `crash_report`（纯函数、有测试），这里只管：
//! - 启动后在后台读一次日志（锁屏时等解锁；别的对话框开着就排队等它关掉）；
//! - 弹一个主窗口自己的对话框（不走系统弹窗）：「反馈问题」「打开日志文件夹」「不用了」；
//! - 「反馈问题」按清洗过的最近几条记录拼地址、交给系统浏览器，「不用了」和 Esc 只关掉；
//!   这两种都把「已读标记」更新到最新一条，同一批记录只提示一次。「打开日志文件夹」不关对话框。
//!
//! ZeeTerm 自己不发网络请求：浏览器里打开的是预填好的 GitHub 页面，用户检查过、改过再自己提交。
//! 只有主窗口提示，独立终端窗口和文件窗口不管。

use ::gpui::{SharedString, Window};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::bridge::on_blocking;
use super::modal::Modal;
use super::widgets::{ButtonLook, text_button};
use crate::app_theme::Tokens;
use crate::crash_report::{self, Pending};
use crate::crash_scrub::Names;
use crate::product;
use crate::typography::{Role, Typo as _};

/// 对话框的状态：启动时发现的、还没处理的记录。
pub struct CrashPrompt {
    pub pending: Pending,
}

/// 崩溃提示的文案。
#[derive(Clone, Copy, Debug)]
enum CrashMsg {
    /// 产品名 + 本键：标题。
    TitleSuffix,
    Body,
    Report,
    OpenLogs,
    Dismiss,
    /// 打不开文件管理器时的提示前缀。
    #[cfg_attr(not(windows), allow(dead_code))]
    OpenLogsFailedPrefix,
}

impl crate::i18n::Msg for CrashMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TitleSuffix => " 上次异常退出了",
            Self::Body => {
                "要把崩溃记录反馈给开发者吗？会在浏览器里打开 GitHub 的反馈页，记录已去掉服务器地址、用户名等信息，提交前你还可以检查和删改。"
            }
            Self::Report => "反馈问题",
            Self::OpenLogs => "打开日志文件夹",
            Self::Dismiss => "不用了",
            Self::OpenLogsFailedPrefix => "没能打开日志文件夹：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TitleSuffix => " exited unexpectedly last time",
            Self::Body => {
                "Send the crash record to the developers? This opens GitHub's feedback page in your browser. Server addresses, user names and similar details have been removed from the record, and you can review and edit it before submitting."
            }
            Self::Report => "Report Problem",
            Self::OpenLogs => "Open Log Folder",
            Self::Dismiss => "Not Now",
            Self::OpenLogsFailedPrefix => "Could not open the log folder: ",
        }
    }
}

impl MainWindow {
    /// 启动后读一次崩溃记录（后台线程读盘）。已经读过就不再读；锁着时由调用方等解锁。
    pub(super) fn scan_crash_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.crash_scanned {
            return;
        }
        self.crash_scanned = true;
        let logs = crash_report::logs_dir(&crate::settings::data_dir());
        let Ok(rx) = on_blocking(move || crash_report::scan(&logs)) else {
            return;
        };
        self.await_result(rx, window, cx, |this, pending, _window, cx| {
            this.crash_pending = pending;
            this.show_crash_prompt_if_ready(cx);
        });
    }

    /// 有待处理的记录、没有别的对话框开着、没锁屏时弹提示；否则留着，下个节拍再试。
    pub(super) fn show_crash_prompt_if_ready(&mut self, cx: &mut Context<Self>) {
        if self.crash_pending.is_none() || self.modal.is_some() || crate::app_lock::is_locked() {
            return;
        }
        let Some(pending) = self.crash_pending.take() else {
            return;
        };
        self.modal = Some(Modal::CrashReport(CrashPrompt { pending }));
        cx.notify();
    }

    /// 「反馈问题」：把清洗过的最近几条记录预填进 GitHub 的新建 issue 页，用浏览器打开，并记下已处理。
    pub(crate) fn crash_prompt_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::CrashReport(prompt)) = self.modal.take() else {
            return;
        };
        let names = Names::collect(self.store.hosts());
        let url = crash_report::report_url(
            &crate::feedback::version(),
            &crate::feedback::os_text(),
            crate::feedback::install_kind(),
            &prompt.pending.records,
            &names,
        );
        cx.open_url(&url);
        mark_seen(&prompt.pending);
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 「不用了」、Esc：关掉，记下已处理（同一批记录不再提示）。
    pub(crate) fn crash_prompt_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::CrashReport(prompt)) = self.modal.take() else {
            return;
        };
        mark_seen(&prompt.pending);
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 「打开日志文件夹」：用系统文件管理器打开 `logs/`；对话框不关，用户看完日志还能回来点「反馈问题」。
    pub(crate) fn crash_prompt_open_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = crash_report::logs_dir(&crate::settings::data_dir());
        // 日志文件夹可能还不存在（记录被删掉了）：先建出来，文件管理器才打得开。
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(windows)]
        if let Err(err) = std::process::Command::new("explorer").arg(&dir).spawn() {
            self.notice_error(
                &format!("{}{err}", crate::i18n::t(CrashMsg::OpenLogsFailedPrefix)),
                window,
                cx,
            );
        }
        // Linux / macOS：GPUI 交给桌面的文件管理器。
        #[cfg(not(windows))]
        {
            let _ = &window;
            cx.open_with_system(&dir);
        }
    }

    pub(super) fn crash_prompt_body(
        &self,
        _prompt: &CrashPrompt,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let (report, logs, dismiss) = (cx.entity(), cx.entity(), cx.entity());
        let title = SharedString::from(format!(
            "{}{}",
            product::NAME,
            crate::i18n::t(CrashMsg::TitleSuffix)
        ));
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(div().typo(Role::GroupTitle, tokens).child(title))
                    .child(
                        div()
                            .typo(Role::Body, tokens)
                            .child(crate::i18n::t(CrashMsg::Body)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "crash-dismiss",
                        crate::i18n::t(CrashMsg::Dismiss),
                        ButtonLook::Ghost,
                        move |_, window, cx| {
                            dismiss.update(cx, |this, cx| this.crash_prompt_dismiss(window, cx));
                        },
                    ))
                    .child(text_button(
                        "crash-open-logs",
                        crate::i18n::t(CrashMsg::OpenLogs),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            logs.update(cx, |this, cx| this.crash_prompt_open_logs(window, cx));
                        },
                    ))
                    .child(text_button(
                        "crash-report",
                        crate::i18n::t(CrashMsg::Report),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            report.update(cx, |this, cx| this.crash_prompt_report(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }
}

/// 把「已读标记」写到这一批记录里最新一条。
fn mark_seen(pending: &Pending) {
    crash_report::mark_seen(
        &crash_report::logs_dir(&crate::settings::data_dir()),
        &pending.latest,
    );
}
