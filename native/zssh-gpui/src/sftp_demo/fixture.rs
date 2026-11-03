//! 调试夹具：`ZSSH_SFTP_FAKE=<远端目录>`（只在 debug 构建里读，见 `dev_hooks`）让文件视图不连任何服务器，
//! 直接当作"已连接"，远端栏显示一份固定的假目录。给界面截图和自动化用：不碰真实服务器，列表内容稳定。
//!
//! 夹具视图没有通道和引擎：换远端目录只是换路径、列表仍是同一份假数据；传输、改名这类要通道的动作
//! 会照常提示"还没连上"。复制标签、移到文件窗口、关标签这些只动视图本身的流程和真实视图走同一条路。

use zssh_core::sftp::path::{join, normalize};
use zssh_core::sftp::{DirEntry, FileKind};

use super::engine::env_value;
use super::model::NavMode;
use gpui_kit::*;

use super::SftpDemo;

/// 夹具的远端起始目录；没设钩子（或者 release 构建）就是 `None`，走真实连接。
/// 写成 `1` 取默认目录。
pub(super) fn start_dir() -> Option<String> {
    env_value("ZSSH_SFTP_FAKE").map(|raw| {
        if raw == "1" {
            "/srv/www".to_owned()
        } else {
            raw
        }
    })
}

fn entry(dir: &str, name: &str, kind: FileKind, size: Option<u64>, mtime: u32) -> DirEntry {
    let link_target =
        matches!(kind, FileKind::Symlink { .. }).then(|| "releases/2026-11-01".to_owned());
    let permissions = match kind {
        FileKind::Dir | FileKind::Symlink { .. } => 0o755,
        _ => 0o644,
    };
    DirEntry {
        name: name.to_owned(),
        path: join(dir, name),
        kind,
        link_target,
        size,
        mtime: Some(mtime),
        permissions: Some(permissions),
        owner: Some("deploy".to_owned()),
        name_is_utf8: true,
    }
}

/// 假目录的内容（每个目录都一样，只有路径不同）。
pub(super) fn entries(dir: &str) -> Vec<DirEntry> {
    let day = 86_400u32;
    let base = 1_730_000_000u32;
    vec![
        entry(dir, "releases", FileKind::Dir, None, base),
        entry(dir, "shared", FileKind::Dir, None, base + day),
        entry(dir, "logs", FileKind::Dir, None, base + 2 * day),
        entry(
            dir,
            "current",
            FileKind::Symlink {
                target: Some(Box::new(FileKind::Dir)),
            },
            None,
            base + 3 * day,
        ),
        entry(
            dir,
            "app.tar.gz",
            FileKind::File,
            Some(48_234_496),
            base + 4 * day,
        ),
        entry(
            dir,
            "big.bin",
            FileKind::File,
            Some(300_032),
            base + 5 * day,
        ),
        entry(
            dir,
            "deploy.sh",
            FileKind::File,
            Some(2_048),
            base + 6 * day,
        ),
        entry(
            dir,
            "nginx.conf",
            FileKind::File,
            Some(1_536),
            base + 7 * day,
        ),
        entry(dir, "README.md", FileKind::File, Some(812), base + 8 * day),
        entry(dir, ".env", FileKind::File, Some(214), base + 9 * day),
    ]
}

impl SftpDemo {
    /// 夹具视图换远端目录：只换路径，列表还是那份假数据。
    pub(super) fn fixture_navigate(&mut self, path: &str, mode: NavMode, cx: &mut Context<Self>) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        let target = normalize(&join(&self.remote.path, trimmed));
        let rows = entries(&target);
        self.set_remote_listing(target, rows, mode, None);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::entries;

    #[test]
    fn the_fixture_lists_the_same_names_under_whatever_directory() {
        let a = entries("/srv/www");
        let b = entries("/srv/www/releases");
        assert_eq!(a.len(), b.len());
        assert!(a.iter().all(|e| e.path.starts_with("/srv/www/")));
        assert!(b.iter().all(|e| e.path.starts_with("/srv/www/releases/")));
        assert!(a.iter().any(|e| e.name == "big.bin"));
    }
}
