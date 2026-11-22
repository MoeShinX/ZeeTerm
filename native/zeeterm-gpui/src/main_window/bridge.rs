//! 把后台任务的结果送回界面线程：耗时的事（生成 RSA 密钥、连服务器装公钥、起转发）放到进程级
//! SSH 运行时上做，界面这边用 `oneshot` 接结果，到了就在主窗口里处理。
//!
//! 为什么是 `oneshot` + `cx.spawn_in`：russh 的 future 必须在 tokio 运行时里跑，但 gpui 的异步
//! 执行器不是 tokio；`oneshot` 的接收端不依赖任何运行时，gpui 的任务可以直接 `await` 它。
//! 窗口关了 `update_in` 会失败，任务自己结束，不会悬着。

use std::future::Future;

use ::gpui::Window;
use gpui_kit::*;
use tokio::sync::oneshot;

use super::MainWindow;

/// 在 SSH 运行时上跑一个异步任务。
pub fn on_ssh_runtime<T, F>(future: F) -> Result<oneshot::Receiver<T>, String>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let runtime = crate::ssh_backend::runtime()?;
    let (tx, rx) = oneshot::channel();
    runtime.spawn(async move {
        let _ = tx.send(future.await);
    });
    Ok(rx)
}

/// 在 SSH 运行时的阻塞线程上跑一段同步代码（SQLite、密钥生成这类 CPU / 磁盘活）。
pub fn on_blocking<T, F>(work: F) -> Result<oneshot::Receiver<T>, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let runtime = crate::ssh_backend::runtime()?;
    let (tx, rx) = oneshot::channel();
    runtime.spawn_blocking(move || {
        let _ = tx.send(work());
    });
    Ok(rx)
}

impl MainWindow {
    /// 等后台结果，到了就交给 `then`（在界面线程里、能改主窗口状态）。
    pub(super) fn await_result<T: 'static>(
        &mut self,
        rx: oneshot::Receiver<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut MainWindow, T, &mut Window, &mut Context<MainWindow>) + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(value) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| then(this, value, window, cx));
            }
        })
        .detach();
    }
}
