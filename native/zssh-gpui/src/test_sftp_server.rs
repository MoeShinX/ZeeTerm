//! 测试 / 截图用的进程内 SSH + SFTP 服务器：只监听本机回环，端口由系统分配（或指定），主机密钥现场生成，
//! 任何用户名 / 密码 / 密钥都收。`/` 对应本机上的一个根目录（调用方给的临时目录），路径不能用 `..` 跑出去。
//!
//! 只实现文件管理需要的那部分 SFTP：`realpath` / `stat` / `lstat` / `opendir` / `readdir` / `mkdir` /
//! `rmdir` / `remove` / `rename`（目标已存在就失败，和 `SSH_FXP_RENAME` 一样）/ `posix-rename@openssh.com`
//! （可以关掉，用来走客户端“没有扩展”的三步覆盖）。不做文件内容读写。`shell` 请求回一个提示符，
//! 让“先开终端再开文件标签”的窗口场景能走通；`exec` 一律拒绝。
//!
//! 这个文件不依赖应用的别的模块：测试用 `#[path]` 以外的普通 `mod` 引入，示例程序
//! `examples/fake_sftp_server.rs` 用 `#[path]` 引同一份，给截图场景起一个独立进程。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, File, FileAttributes, Handle, Name, Packet, Status, StatusCode, Version,
};
use tokio::net::TcpListener;

/// 服务器的行为开关。
#[derive(Clone, Copy)]
pub struct Options {
    /// 声明并支持 `posix-rename@openssh.com`。关掉后客户端覆盖式改名走“备用名”三步。
    pub posix_rename: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { posix_rename: true }
    }
}

/// 起服务器（接任意多个连接），返回监听地址。`port == 0` 由系统分配。
pub async fn spawn(root: PathBuf, port: u16, options: Options) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![
            PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
                .map_err(|err| std::io::Error::other(format!("生成主机密钥失败：{err}")))?,
        ],
        ..Default::default()
    });
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            let handler = Ssh {
                root: root.clone(),
                options,
                channels: HashMap::new(),
            };
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, socket, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    Ok(addr)
}

struct Ssh {
    root: PathBuf,
    options: Options,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"files-1$ ".to_vec())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            return session.channel_failure(channel_id);
        }
        let Some(channel) = self.channels.remove(&channel_id) else {
            return session.channel_failure(channel_id);
        };
        session.channel_success(channel_id)?;
        let sftp = Sftp {
            root: self.root.clone(),
            options: self.options,
            listed: HashSet::new(),
        };
        tokio::spawn(russh_sftp::server::run(channel.into_stream(), sftp));
        Ok(())
    }
}

struct Sftp {
    root: PathBuf,
    options: Options,
    /// 已经把内容读完的目录句柄（再读就回 EOF）。
    listed: HashSet<String>,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_owned(),
        language_tag: "en-US".to_owned(),
    }
}

fn code_of(err: &std::io::Error) -> StatusCode {
    match err.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

/// 把远端路径规范成以 `/` 开头、没有 `.` / `..` 的形式；想跑出根目录返回 `None`。
fn clean(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for piece in path.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

impl Sftp {
    fn real(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let cleaned = clean(path).ok_or(StatusCode::PermissionDenied)?;
        let mut out = self.root.clone();
        for piece in Path::new(cleaned.trim_start_matches('/')).components() {
            match piece {
                Component::Normal(name) => out.push(name),
                _ => return Err(StatusCode::PermissionDenied),
            }
        }
        Ok(out)
    }

    fn rename_inner(&self, from: &str, to: &str, replace: bool) -> Result<(), StatusCode> {
        let (from, to) = (self.real(from)?, self.real(to)?);
        if !replace && std::fs::symlink_metadata(&to).is_ok() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(&from, &to).map_err(|err| code_of(&err))
    }
}

/// `posix-rename@openssh.com` 的载荷：两个 SSH string。
fn two_strings(data: &[u8]) -> Option<(String, String)> {
    fn take(data: &[u8]) -> Option<(&[u8], &[u8])> {
        let len = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let body = data.get(4..4 + len)?;
        Some((body, &data[4 + len..]))
    }
    let (first, rest) = take(data)?;
    let (second, _) = take(rest)?;
    Some((
        String::from_utf8(first.to_vec()).ok()?,
        String::from_utf8(second.to_vec()).ok()?,
    ))
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        if self.options.posix_rename {
            version
                .extensions
                .insert("posix-rename@openssh.com".to_owned(), "1".to_owned());
        }
        Ok(version)
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.listed.remove(&handle);
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let cleaned = clean(&path).ok_or(StatusCode::PermissionDenied)?;
        Ok(Name {
            id,
            files: vec![File::dummy(cleaned)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let meta = std::fs::metadata(self.real(&path)?).map_err(|err| code_of(&err))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let meta = std::fs::symlink_metadata(self.real(&path)?).map_err(|err| code_of(&err))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let real = self.real(&path)?;
        let meta = std::fs::metadata(&real).map_err(|err| code_of(&err))?;
        if !meta.is_dir() {
            return Err(StatusCode::Failure);
        }
        Ok(Handle {
            id,
            handle: clean(&path).ok_or(StatusCode::PermissionDenied)?,
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        if !self.listed.insert(handle.clone()) {
            return Err(StatusCode::Eof);
        }
        let real = self.real(&handle)?;
        let mut files = Vec::new();
        for item in std::fs::read_dir(real).map_err(|err| code_of(&err))? {
            let item = item.map_err(|err| code_of(&err))?;
            let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
                continue;
            };
            files.push(File::new(
                item.file_name().to_string_lossy().into_owned(),
                FileAttributes::from(&meta),
            ));
        }
        Ok(Name { id, files })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.real(&path)?).map_err(|err| code_of(&err))?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.real(&path)?).map_err(|err| code_of(&err))?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.real(&filename)?).map_err(|err| code_of(&err))?;
        Ok(ok(id))
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    /// `SSH_FXP_RENAME`：目标已存在就失败（只回一个光秃秃的 Failure，客户端自己去看目标在不在）。
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.rename_inner(&oldpath, &newpath, false)?;
        Ok(ok(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        if request == "posix-rename@openssh.com" && self.options.posix_rename {
            let (from, to) = two_strings(&data).ok_or(StatusCode::BadMessage)?;
            self.rename_inner(&from, &to, true)?;
            return Ok(Packet::Status(ok(id)));
        }
        Err(StatusCode::OpUnsupported)
    }
}

/// 临时根目录：随机唯一的名字，Drop 时整个删掉（第二个字段是守卫），断言失败也不留垃圾。
pub struct TempRoot(pub PathBuf, tempfile::TempDir);

impl TempRoot {
    pub fn new(tag: &str) -> Self {
        let guard = tempfile::Builder::new()
            .prefix(&format!("zssh-fake-sftp-{tag}-"))
            .tempdir()
            .expect("建临时根目录");
        Self(guard.path().to_path_buf(), guard)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// 在根目录里按相对路径（`/` 分隔）写一个文件，必要时建好上级。
    pub fn write(&self, relative: &str, body: &[u8]) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建上级目录");
        }
        std::fs::write(path, body).expect("写文件");
    }

    pub fn mkdir(&self, relative: &str) {
        std::fs::create_dir_all(self.0.join(relative)).expect("建目录");
    }

    pub fn read(&self, relative: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(relative)).ok()
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.0.join(relative).exists()
    }
}
