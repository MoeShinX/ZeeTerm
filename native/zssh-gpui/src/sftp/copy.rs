//! 复制 / 粘贴在文件视图里的动作：Ctrl+C / Ctrl+V 和菜单「复制」「粘贴」都落到这里。
//!
//! 复制只记路径（应用内剪贴板，所有 SFTP 视图共用，不碰系统剪贴板）；粘贴在后台跑
//! [`super::paste::run_paste`]：服务器端复制做完直接回报，中转 / 本地复制排进传输队列。
//! 远端和本地之间不能粘贴（请用上传 / 下载，同 0.2.14）。

use std::sync::Arc;

use gpui_kit::*;

use super::archive_ui::BusyKind;
use super::clip::{self, ClipContent, ClipItem, ClipSource, NoticeKind, Outcome};
use super::menu::{MenuContext, MenuFeatures, Side, paste_reason};
use super::model::Entry;
use super::paste::{PasteCtx, PasteRequest, PastedJob, run_paste};
use super::{SftpDemo, SftpEvent, SftpMsg, TransferRow};
use crate::i18n::t;
use crate::ssh_backend::runtime;

/// `cp` 失败后的"改为经本机中转重试"：条上显示原因和两个按钮。
pub(super) struct PasteRetry {
    pub(super) text: String,
    pub(super) items: Vec<ClipItem>,
    pub(super) side: Side,
    pub(super) target_dir: String,
    pub(super) source: ClipSource,
}

/// 往 `target_dir` 粘贴为什么不行；`None` = 可以。和菜单里「粘贴」置灰用同一条规则。
pub(super) fn paste_block_reason(side: Side, target_dir: &str) -> Option<String> {
    paste_reason(&MenuContext {
        side,
        blank: true,
        entries: Vec::new(),
        current_dir: target_dir.to_owned(),
        clipboard: clip::summary(),
        features: MenuFeatures {
            builtin_editor: false,
            clipboard: true,
            properties: false,
        },
        archive: None,
    })
}

/// 最近一次发起的粘贴：失败后的"改为经本机中转重试"要用它的目标和源。
pub(super) struct LastPaste {
    pub(super) side: Side,
    pub(super) target_dir: String,
    pub(super) source: ClipSource,
}

impl SftpDemo {
    /// Ctrl+C / 菜单「复制」：记下选中的条目（名字不是 UTF-8 的不放进去）。
    pub(super) fn copy_selection(&mut self, side: Side, entries: &[Entry]) {
        let items = clip::items_from(entries);
        if items.is_empty() {
            return self.warn(t(SftpMsg::CopySelectionNotUtf8));
        }
        let remote = side == Side::Remote;
        let source = ClipSource {
            view: self.view_id,
            server_key: remote
                .then(|| self.channel.as_ref().map(|c| c.server_key().to_owned()))
                .flatten(),
            provider: remote
                .then(|| self.provider.as_ref().map(Arc::downgrade))
                .flatten(),
        };
        let count = items.len();
        clip::set(ClipContent {
            side,
            items,
            source,
        });
        self.say(format!(
            "{}{}",
            t(SftpMsg::ClipCopiedPrefix),
            crate::i18n::count(count as u64, t(SftpMsg::ItemCountSuffix), "item", "items")
        ));
    }

    /// Ctrl+V / 菜单「粘贴」：粘贴进这一栏的当前目录。
    pub(super) fn paste_here(&mut self, side: Side, cx: &mut Context<Self>) {
        let dir = self.pane(side).path.clone();
        self.paste_into(side, dir, cx);
    }

    /// 粘贴进指定文件夹（右键在文件夹上粘贴）。
    pub(super) fn paste_into(&mut self, side: Side, target_dir: String, cx: &mut Context<Self>) {
        if let Some(reason) = paste_block_reason(side, &target_dir) {
            return self.warn(reason);
        }
        let Some(content) = clip::get() else {
            return self.warn(t(SftpMsg::MenuClipboardEmpty));
        };
        self.start_paste(content.items, side, target_dir, content.source, false, cx);
    }

    /// 真正发起一次粘贴（首次、或者 `cp` 失败后点"改为经本机中转重试"都走这里）。
    pub(super) fn start_paste(
        &mut self,
        items: Vec<ClipItem>,
        side: Side,
        target_dir: String,
        source: ClipSource,
        force_relay: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            // 提示条只有一条：服务器上正在压缩 / 解压 / 复制时不再开始，免得盖掉它的取消按钮。
            return self.warn(t(SftpMsg::OpsServerBusy));
        }
        let (Some(browse), Some(factory), Some(engine), Some(endpoints), Some(resolver)) = (
            self.channel.clone(),
            self.factory.clone(),
            self.engine.clone(),
            self.endpoints.clone(),
            self.resolver.clone(),
        ) else {
            return self.warn(t(SftpMsg::NotConnectedYet));
        };
        let Ok(runtime) = runtime() else {
            return self.warn(t(SftpMsg::OpsNoRuntime));
        };
        let Some((op, cancel)) = self.begin_busy(BusyKind::Paste) else {
            return;
        };
        if let Some(busy) = self.busy.as_mut() {
            busy.local = side == Side::Local;
        }
        self.paste_retry = None;
        self.last_paste = Some(LastPaste {
            side,
            target_dir: target_dir.clone(),
            source: source.clone(),
        });
        let ctx = PasteCtx {
            browse,
            connection: factory.connection(),
            engine,
            endpoints,
            resolver,
            cancel: Arc::clone(&cancel),
            sender: self.sender.clone(),
            shell_kind: Arc::clone(&self.shell_kind),
            no_copy_data: crate::dev_hooks::var("ZSSH_PASTE_NO_COPY_DATA").is_some(),
        };
        let request = PasteRequest {
            op,
            force_relay,
            side,
            items,
            target_dir,
            source,
            this_view: self.view_id,
        };
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let result = run_paste(ctx, request).await;
            let _ = sender.send(SftpEvent::PasteDone { op, result });
        });
        cx.notify();
    }

    /// 粘贴做完了：排进引擎的任务建成队列里的行，结果写进状态栏，`cp` 失败的给"改为经本机中转重试"。
    pub(super) fn finish_paste(
        &mut self,
        op: u64,
        result: Result<(Outcome, Vec<PastedJob>), String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy.as_ref().is_some_and(|busy| busy.op == op) {
            self.busy = None;
        }
        let side = self
            .last_paste
            .as_ref()
            .map_or(Side::Remote, |last| last.side);
        let (outcome, jobs) = match result {
            Ok(done) => done,
            Err(message) => {
                self.warn(format!("{}{message}", t(SftpMsg::OpsPasteFailedPrefix)));
                return;
            }
        };
        let queued = !jobs.is_empty();
        for job in jobs {
            let row = TransferRow::new(
                job.id,
                job.name,
                job.direction,
                job.batch,
                job.source,
                job.target,
            );
            // 比行先到的进度补上（小文件可能已经做完了）。
            let early = self.orphan_reports.remove(&job.id);
            self.transfers.push(row);
            if let Some(report) = early {
                self.apply_report(report);
            }
        }
        self.sync_install_state();
        if queued {
            self.queue_open = true;
        }
        let notice = clip::notice(&outcome);
        match notice.kind {
            NoticeKind::Error => self.warn(notice.text.clone()),
            _ => self.say(notice.text.clone()),
        }
        // cp 跑了但失败：给"改为经本机中转重试"，点了才换办法。
        if let Some(items) = clip::relay_retry_items(&outcome)
            && let Some(last) = self.last_paste.as_ref()
        {
            self.paste_retry = Some(PasteRetry {
                text: notice.text.clone(),
                items,
                side,
                target_dir: last.target_dir.clone(),
                source: last.source.clone(),
            });
        }
        // 服务器端复制已经改了目录：两栏都刷新（中转 / 本地复制等任务做完时再刷新一次）。
        let target_side = side;
        self.refresh_with_notice(target_side, notice.text, cx);
        cx.notify();
    }

    /// 条上的"改为经本机中转重试"。
    pub(super) fn retry_paste_via_relay(&mut self, cx: &mut Context<Self>) {
        let Some(retry) = self.paste_retry.take() else {
            return;
        };
        self.start_paste(
            retry.items,
            retry.side,
            retry.target_dir,
            retry.source,
            true,
            cx,
        );
    }

    pub(super) fn dismiss_paste_retry(&mut self, cx: &mut Context<Self>) {
        self.paste_retry = None;
        cx.notify();
    }
}
