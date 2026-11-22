//! W06：探测 SFTP 服务器认不认独占创建标志（`server_honors_exclusive`）。用进程内的假服务器，
//! 不连任何真实机器。认的服务器必须拒绝对已有文件的独占打开；不认的会放行——后者不能拿来
//! 原子地占位新建的目标。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, Handle, OpenFlags, Status, StatusCode, Version};
use russh_sftp::server::StatusReply;

use super::*;

struct State {
    files: HashMap<String, Vec<u8>>,
    honor_excl: bool,
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
        _attrs: FileAttributes,
    ) -> Result<Handle, StatusReply> {
        let mut state = self.0.lock().unwrap();
        let exists = state.files.contains_key(&filename);
        if exists && pflags.contains(OpenFlags::EXCLUDE) && state.honor_excl {
            // OpenSSH 的做法：只回笼统的 Failure。
            return Err(StatusCode::Failure.with_message("Failure"));
        }
        if exists && pflags.contains(OpenFlags::TRUNCATE) {
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
}

async fn session(
    honor_excl: bool,
    existing: &[(&str, &[u8])],
) -> (RawSftpSession, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        files: existing
            .iter()
            .map(|(name, data)| ((*name).to_owned(), data.to_vec()))
            .collect(),
        honor_excl,
    }));
    let (client, server) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server, Fake(Arc::clone(&state))).await;
    let raw = RawSftpSession::new(client);
    raw.init().await.expect("握手");
    (raw, state)
}

#[tokio::test]
async fn a_server_that_honors_the_flag_refuses_the_second_exclusive_open() {
    let (raw, state) = session(true, &[("/srv/a", b"")]).await;
    assert!(server_honors_exclusive(&raw, "/srv/a").await.unwrap());
    assert_eq!(state.lock().unwrap().files["/srv/a"], b"");
}

#[tokio::test]
async fn a_server_that_ignores_the_flag_is_detected_and_the_file_is_left_alone() {
    let (raw, state) = session(false, &[("/srv/a", b"data")]).await;
    assert!(
        !server_honors_exclusive(&raw, "/srv/a").await.unwrap(),
        "放行了独占打开 = 不认标志"
    );
    assert_eq!(
        state.lock().unwrap().files["/srv/a"],
        b"data",
        "探测不带 TRUNCATE，不能动内容"
    );
}
