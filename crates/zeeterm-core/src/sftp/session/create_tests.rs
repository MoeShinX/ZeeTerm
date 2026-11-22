//! M-5：新建文件不能清空已经存在的同名文件。用进程内的假 SFTP 服务器（内存里的文件表），
//! 覆盖三种服务器：认独占标志的、忽略独占标志的、两次探测之间有人抢先建了文件的。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh_sftp::protocol::{Attrs, FileAttributes, Handle, OpenFlags, Status, StatusCode, Version};
use russh_sftp::server::StatusReply;

use super::*;

#[derive(Default)]
struct State {
    files: HashMap<String, Vec<u8>>,
    /// 认不认 `EXCLUDE`：不认的服务器会把已有文件当成普通打开。
    honor_excl: bool,
    /// 第一次 `lstat` 这个路径时回“不存在”，随后这个路径上冒出一个文件（模拟被抢先建了）。
    appears_after_first_lstat: Option<String>,
    /// 打开时返回权限不足。
    deny_open: bool,
    /// 收到过几次带 `TRUNCATE` 的打开，且打开的是已存在的文件。
    truncations: usize,
    opens: usize,
    /// 最近一次打开请求里带的创建权限位。
    last_open_permissions: Option<u32>,
}

struct Fake(Arc<Mutex<State>>);

impl russh_sftp::server::Handler for Fake {
    type Error = StatusReply;

    fn unimplemented(&self) -> StatusReply {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, StatusReply> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, StatusReply> {
        let mut state = self.0.lock().expect("锁");
        state.opens += 1;
        state.last_open_permissions = attrs.permissions;
        if state.deny_open {
            return Err(StatusReply::new(StatusCode::PermissionDenied));
        }
        let exists = state.files.contains_key(&filename);
        if exists && pflags.contains(OpenFlags::EXCLUDE) && state.honor_excl {
            // OpenSSH 的做法：只回笼统的 Failure。
            return Err(StatusCode::Failure.with_message("Failure"));
        }
        if exists && pflags.contains(OpenFlags::TRUNCATE) {
            state.truncations += 1;
            state.files.insert(filename.clone(), Vec::new());
        }
        state.files.entry(filename.clone()).or_default();
        Ok(Handle {
            id,
            handle: filename,
        })
    }

    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, StatusReply> {
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        let mut state = self.0.lock().expect("锁");
        if state.appears_after_first_lstat.as_deref() == Some(path.as_str()) {
            state.appears_after_first_lstat = None;
            state.files.insert(path, b"someone else's data".to_vec());
            return Err(StatusReply::new(StatusCode::NoSuchFile));
        }
        match state.files.get(&path) {
            Some(data) => Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(data.len() as u64),
                    ..Default::default()
                },
            }),
            None => Err(StatusReply::new(StatusCode::NoSuchFile)),
        }
    }
}

async fn session(state: State) -> (RawSftpSession, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(state));
    let (client, server) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server, Fake(Arc::clone(&state))).await;
    let raw = RawSftpSession::new(client);
    raw.init().await.expect("握手");
    (raw, state)
}

fn content(state: &Arc<Mutex<State>>, path: &str) -> Option<Vec<u8>> {
    state.lock().expect("锁").files.get(path).cloned()
}

#[tokio::test]
async fn a_missing_file_is_created_empty() {
    let (raw, state) = session(State {
        honor_excl: true,
        ..Default::default()
    })
    .await;
    create_new_file(&raw, "/srv/new.txt").await.expect("新建");
    assert_eq!(content(&state, "/srv/new.txt"), Some(Vec::new()));
}

#[tokio::test]
async fn an_existing_file_is_reported_and_keeps_its_content() {
    let mut files = HashMap::new();
    files.insert("/srv/a.txt".to_owned(), b"precious".to_vec());
    let (raw, state) = session(State {
        files,
        honor_excl: true,
        ..Default::default()
    })
    .await;
    let err = create_new_file(&raw, "/srv/a.txt")
        .await
        .expect_err("已存在要报错");
    assert!(matches!(err, SftpError::AlreadyExists { .. }), "{err:?}");
    assert_eq!(content(&state, "/srv/a.txt"), Some(b"precious".to_vec()));
    assert_eq!(state.lock().unwrap().truncations, 0);
}

/// 服务器忽略独占标志：只靠独占标志会把文件清空，所以必须先看一眼。
#[tokio::test]
async fn a_server_that_ignores_the_exclusive_flag_still_cannot_truncate_it() {
    let mut files = HashMap::new();
    files.insert("/srv/a.txt".to_owned(), b"precious".to_vec());
    let (raw, state) = session(State {
        files,
        honor_excl: false,
        ..Default::default()
    })
    .await;
    let err = create_new_file(&raw, "/srv/a.txt")
        .await
        .expect_err("已存在要报错");
    assert!(matches!(err, SftpError::AlreadyExists { .. }), "{err:?}");
    assert_eq!(content(&state, "/srv/a.txt"), Some(b"precious".to_vec()));
    let state = state.lock().unwrap();
    assert_eq!(state.truncations, 0);
    assert_eq!(state.opens, 0, "探测到已存在就不该再发 open");
}

/// 探测和创建之间被别人抢先建了文件：独占创建的笼统失败要被认成“已存在”，内容不动。
#[tokio::test]
async fn a_file_created_between_the_probe_and_the_open_is_reported_as_existing() {
    let (raw, state) = session(State {
        honor_excl: true,
        appears_after_first_lstat: Some("/srv/race.txt".to_owned()),
        ..Default::default()
    })
    .await;
    let err = create_new_file(&raw, "/srv/race.txt")
        .await
        .expect_err("被抢先建了要报错");
    assert!(matches!(err, SftpError::AlreadyExists { .. }), "{err:?}");
    assert_eq!(
        content(&state, "/srv/race.txt"),
        Some(b"someone else's data".to_vec())
    );
}

/// 打开失败的原因不是“已存在”时，原样报，不能冒充成已存在。
#[tokio::test]
async fn other_open_failures_keep_their_own_error() {
    let (raw, state) = session(State {
        honor_excl: true,
        deny_open: true,
        ..Default::default()
    })
    .await;
    let err = create_new_file(&raw, "/srv/nope.txt")
        .await
        .expect_err("没权限");
    assert!(matches!(err, SftpError::PermissionDenied { .. }), "{err:?}");
    assert_eq!(content(&state, "/srv/nope.txt"), None);
}

/// 上传 0600 的私钥：创建请求里要带权限位，服务器才不会按默认 0644 建出一个更宽的临时文件。
#[tokio::test]
async fn opening_for_write_carries_the_creation_mode() {
    let (raw, state) = session(State::default()).await;
    open_for_write(
        &raw,
        "/srv/key.part",
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        Some(0o100600),
    )
    .await
    .expect("打开");
    // 只带 rwx 九位：文件类型位和 setuid 等特殊位不往服务器发。
    assert_eq!(state.lock().unwrap().last_open_permissions, Some(0o600));

    open_for_write(&raw, "/srv/key.part", OpenFlags::WRITE, Some(0o4755))
        .await
        .expect("打开");
    assert_eq!(state.lock().unwrap().last_open_permissions, Some(0o755));
}

/// 没给权限位（Windows 来源等）：和以前一样什么都不带，由服务器按默认处理。
#[tokio::test]
async fn opening_for_write_without_a_mode_sends_no_permissions() {
    let (raw, state) = session(State::default()).await;
    open_for_write(
        &raw,
        "/srv/a.bin",
        OpenFlags::WRITE | OpenFlags::CREATE,
        None,
    )
    .await
    .expect("打开");
    assert_eq!(state.lock().unwrap().last_open_permissions, None);
}
