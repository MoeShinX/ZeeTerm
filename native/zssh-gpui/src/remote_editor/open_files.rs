//! 进程里有哪些远端文件正开在编辑器里（主窗口的编辑器标签或独立编辑窗口）。
//!
//! 文件视图的「移动到…」要跳过正在编辑的项（改了位置，编辑器保存时会写回旧路径，等于悄悄建出
//! 一个新文件）。文件视图和编辑器是两个不相干的实体，视图在更新自己的时候不能去读主窗口里的
//! 标签（主窗口可能正好在更新），所以编辑器打开时在这里登记、关闭（被丢弃）时撤销，视图只查这张表。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use zssh_core::sftp::path::normalize;

struct Slot {
    id: u64,
    host_id: i64,
    /// 规范化之后的远端路径。
    path: String,
}

static OPEN: Mutex<Vec<Slot>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock() -> std::sync::MutexGuard<'static, Vec<Slot>> {
    // 登记表只是个提示：别的线程 panic 留下的毒不影响数据本身。
    OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一个编辑器占着的登记；编辑器丢掉它（关标签 / 关窗口）登记就撤销。
pub(crate) struct OpenFile {
    id: u64,
}

impl OpenFile {
    pub(crate) fn register(host_id: i64, path: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        lock().push(Slot {
            id,
            host_id,
            path: normalize(path),
        });
        Self { id }
    }

    /// 编辑器改跟踪另一个文件（另存为之后）。
    pub(crate) fn relocate(&self, path: &str) {
        if let Some(slot) = lock().iter_mut().find(|slot| slot.id == self.id) {
            slot.path = normalize(path);
        }
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        lock().retain(|slot| slot.id != self.id);
    }
}

/// 这台主机上，`path` 本身或（它是文件夹时）它里面的某个文件正开在编辑器里吗？
/// 返回第一个这样的文件的完整路径。
pub(crate) fn open_at_or_under(host_id: i64, path: &str) -> Option<String> {
    let wanted = normalize(path);
    let prefix = if wanted.ends_with('/') {
        wanted.clone()
    } else {
        format!("{wanted}/")
    };
    lock()
        .iter()
        .find(|slot| {
            slot.host_id == host_id && (slot.path == wanted || slot.path.starts_with(&prefix))
        })
        .map(|slot| slot.path.clone())
}

#[cfg(test)]
mod tests {
    use super::{OpenFile, open_at_or_under};

    // 登记表是进程级的，各条测试用自己的主机号，互不干扰。

    #[test]
    fn a_registered_file_is_found_until_its_editor_is_dropped() {
        let guard = OpenFile::register(9_001, "/srv/app/main.rs");
        assert_eq!(
            open_at_or_under(9_001, "/srv/app/main.rs").as_deref(),
            Some("/srv/app/main.rs")
        );
        drop(guard);
        assert_eq!(open_at_or_under(9_001, "/srv/app/main.rs"), None);
    }

    #[test]
    fn a_folder_counts_when_a_file_inside_it_is_open_but_a_sibling_prefix_does_not() {
        let _guard = OpenFile::register(9_002, "/srv/app/sub/a.txt");
        assert_eq!(
            open_at_or_under(9_002, "/srv/app").as_deref(),
            Some("/srv/app/sub/a.txt")
        );
        assert_eq!(
            open_at_or_under(9_002, "/srv/app/").as_deref(),
            Some("/srv/app/sub/a.txt")
        );
        // `/srv/ap` 只是字符串前缀，不是上级目录。
        assert_eq!(open_at_or_under(9_002, "/srv/ap"), None);
        assert_eq!(open_at_or_under(9_002, "/srv/app/sub/a.txt.bak"), None);
    }

    #[test]
    fn hosts_are_kept_apart_and_paths_are_compared_normalized() {
        let _guard = OpenFile::register(9_003, "/srv//app/./b.txt");
        assert!(open_at_or_under(9_003, "/srv/app/b.txt").is_some());
        assert_eq!(open_at_or_under(9_004, "/srv/app/b.txt"), None);
    }

    #[test]
    fn relocating_follows_the_editor_to_its_new_file() {
        let guard = OpenFile::register(9_005, "/srv/old.txt");
        guard.relocate("/srv/new.txt");
        assert_eq!(open_at_or_under(9_005, "/srv/old.txt"), None);
        assert!(open_at_or_under(9_005, "/srv/new.txt").is_some());
    }
}
