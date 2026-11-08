//! 编辑器的文件契约：读取快照、版本校验、冲突检测、原子保存。
//!
//! 这里先用本地文件实现，形状对齐现有 `src-tauri` 的保存契约：
//! - 打开时记录内容版本；
//! - 保存时带上打开时的版本，文件被别人改过就返回 `Conflict`，不覆盖；
//! - 保存走临时文件 + 重命名，失败不会留下半份文件；
//! - 拒绝二进制和超大文件。
//!
//! 远端 SFTP 版在 G04/G09 接同一组类型；本 crate 不依赖 Tauri/GPUI/SSH。

use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 单文件上限：超过就拒绝打开，避免把编辑器拖死。
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileVersion(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub text: String,
    pub version: FileVersion,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(FileVersion),
    Conflict { current: FileVersion },
}

#[derive(Debug)]
pub enum EditorError {
    TooLarge(usize),
    Binary,
    NotUtf8,
    Io(String),
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(size) => write!(f, "文件太大（{size} 字节），拒绝打开"),
            Self::Binary => write!(f, "这是二进制文件，拒绝打开"),
            Self::NotUtf8 => write!(f, "文件不是 UTF-8 文本"),
            Self::Io(err) => write!(f, "文件操作失败：{err}"),
        }
    }
}

impl std::error::Error for EditorError {}

pub fn read(path: &Path) -> Result<FileSnapshot, EditorError> {
    let bytes = fs::read(path).map_err(io_error)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(EditorError::TooLarge(bytes.len()));
    }
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(EditorError::Binary);
    }
    let text = String::from_utf8(bytes.clone()).map_err(|_| EditorError::NotUtf8)?;
    Ok(FileSnapshot {
        text,
        version: version_of(&bytes),
    })
}

/// 保存：版本不一致返回 `Conflict`，不写盘；一致才原子替换。
pub fn save(path: &Path, text: &str, expected: FileVersion) -> Result<SaveOutcome, EditorError> {
    let current = fs::read(path).map_err(io_error)?;
    if version_of(&current) != expected {
        return Ok(SaveOutcome::Conflict {
            current: version_of(&current),
        });
    }

    let bytes = text.as_bytes();
    if bytes.len() > MAX_FILE_BYTES {
        return Err(EditorError::TooLarge(bytes.len()));
    }

    let temp = temp_path(path);
    fs::write(&temp, bytes).map_err(io_error)?;
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(err));
    }
    Ok(SaveOutcome::Saved(version_of(bytes)))
}

fn version_of(bytes: &[u8]) -> FileVersion {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    FileVersion(hasher.finish())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".zssh-tmp");
    path.with_file_name(name)
}

fn io_error(err: std::io::Error) -> EditorError {
    EditorError::Io(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 返回的 `TempDir` 要在用例结束前一直拿着：目录名随机唯一，drop 时整棵删掉。
    fn temp_file(name: &str, contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("zssh-editor-{name}-"))
            .tempdir()
            .expect("创建临时目录");
        let path = dir.path().join("sample.txt");
        fs::write(&path, contents).expect("写初始文件");
        (dir, path)
    }

    #[test]
    fn read_then_save_roundtrip() {
        let (_dir, path) = temp_file("roundtrip", "hello\nworld\n".as_bytes());
        let snapshot = read(&path).expect("读取");
        assert_eq!(snapshot.text, "hello\nworld\n");

        let outcome = save(&path, "hello\nnative\n", snapshot.version).expect("保存");
        assert!(matches!(outcome, SaveOutcome::Saved(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\nnative\n");
    }

    #[test]
    fn external_change_is_a_conflict_and_is_not_overwritten() {
        let (_dir, path) = temp_file("conflict", b"original");
        let snapshot = read(&path).expect("读取");
        fs::write(&path, b"changed by someone else").unwrap();

        let outcome = save(&path, "my edit", snapshot.version).expect("保存");
        assert!(matches!(outcome, SaveOutcome::Conflict { .. }));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "changed by someone else",
            "冲突时不能覆盖别人的内容"
        );
    }

    #[test]
    fn binary_and_oversized_files_are_rejected() {
        let (_dir_binary, binary) = temp_file("binary", b"abc\0def");
        assert!(matches!(read(&binary), Err(EditorError::Binary)));

        let (_dir_big, big) = temp_file("big", &vec![b'a'; MAX_FILE_BYTES + 1]);
        assert!(matches!(read(&big), Err(EditorError::TooLarge(_))));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let (_dir, path) = temp_file("utf8", &[0xff, 0xfe, 0xfa]);
        assert!(matches!(read(&path), Err(EditorError::NotUtf8)));
    }

    #[test]
    fn save_does_not_leave_temp_files_on_success() {
        let (_dir, path) = temp_file("temp-clean", b"a");
        let snapshot = read(&path).unwrap();
        save(&path, "b", snapshot.version).unwrap();
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("zssh-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
