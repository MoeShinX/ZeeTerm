//! 本机"发布页"夹具：把一个目录当成 GitHub Release 的下载目录，只在 127.0.0.1 上听。
//! 给人工 / 截图验证更新流程用（单元测试里有自己的内置小服务）。
//!
//! ```text
//! cargo run --release --manifest-path native/Cargo.toml -p zssh-update --example fixture_server -- \
//!     --dir <目录> [--port <端口>] [--rate <字节/秒>] [--cut-after <字节>]
//! ```
//!
//! - 目录里的文件按相对路径提供（例如 `latest.json`、`download/v1.2.3/ZeeTerm-Setup-1.2.3.exe`），没有的路径回 404；
//! - `--port`：固定端口（不给就让系统挑一个空闲的）；
//! - `--rate`：`.exe` 文件按这个速度慢慢发，让"下载中"的界面停得住；
//! - `--cut-after`：`.exe` 文件发到这个字节数就断开（模拟下载中断）。
//!
//! 启动后打印一行 `READY http://127.0.0.1:<端口>`，之后每收到一个请求打印一行 `REQ <路径>`（用来核对"没配公钥时不发请求"这类事），随后一直运行，直到进程被结束。只供本机调试。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

fn main() {
    let mut dir = None;
    let mut rate = None::<u64>;
    let mut cut_after = None::<usize>;
    let mut port = 0u16;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_default();
        match flag.as_str() {
            "--dir" => dir = Some(PathBuf::from(value)),
            "--rate" => rate = value.parse().ok(),
            "--cut-after" => cut_after = value.parse().ok(),
            "--port" => port = value.parse().unwrap_or(0),
            other => {
                eprintln!("不认识的参数：{other}");
                std::process::exit(2);
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("缺少 --dir <目录>");
        std::process::exit(2);
    };
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("绑定本地端口");
    let port = listener.local_addr().expect("端口").port();
    println!("READY http://127.0.0.1:{port}");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming().flatten() {
        let dir = dir.clone();
        std::thread::spawn(move || serve(stream, &dir, rate, cut_after));
    }
}

/// 请求路径换成目录里的文件；带 `..` 之类的路径一律当没有。
fn resolve(dir: &Path, request_path: &str) -> Option<PathBuf> {
    let relative = Path::new(request_path.trim_start_matches('/'));
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let path = dir.join(relative);
    path.is_file().then_some(path)
}

fn serve(mut stream: TcpStream, dir: &Path, rate: Option<u64>, cut_after: Option<usize>) {
    let mut seen = Vec::new();
    let mut buffer = [0u8; 1024];
    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => seen.extend_from_slice(&buffer[..read]),
        }
    }
    let request = String::from_utf8_lossy(&seen).into_owned();
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");
    println!("REQ {path}");
    let _ = std::io::stdout().flush();
    let Some(file) = resolve(dir, path) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let Ok(body) = std::fs::read(&file) else {
        return;
    };
    let is_exe = file
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let limit = match (is_exe, cut_after) {
        (true, Some(cut)) => cut.min(body.len()),
        _ => body.len(),
    };
    // 慢速发送：每 50 毫秒发一小块，凑成大约 `rate` 字节/秒。
    let chunk = match (is_exe, rate) {
        (true, Some(rate)) => usize::try_from(rate / 20).unwrap_or(1).max(1),
        _ => body.len().max(1),
    };
    for piece in body[..limit].chunks(chunk) {
        if stream.write_all(piece).is_err() {
            return;
        }
        if is_exe && rate.is_some() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = stream.flush();
}
