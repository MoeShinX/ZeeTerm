//! 截图 / 手动验证用的独立假 SFTP 服务器进程（只绑回环，什么密码 / 密钥都收，不碰网络之外的任何东西）：
//!
//! ```text
//! cargo run --manifest-path native/Cargo.toml -p zssh-gpui --example fake_sftp_server -- <根目录> <端口> [--no-posix-rename]
//! ```
//!
//! `/` 对应 `<根目录>`；实现和单元测试用的是同一份（`src/test_sftp_server.rs`）。

#[path = "../src/test_sftp_server.rs"]
mod test_sftp_server;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(port)) = (args.next(), args.next().and_then(|p| p.parse::<u16>().ok()))
    else {
        eprintln!("用法：fake_sftp_server <根目录> <端口> [--no-posix-rename]");
        std::process::exit(2);
    };
    let options = test_sftp_server::Options {
        posix_rename: !args.any(|arg| arg == "--no-posix-rename"),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("起运行时失败：{err}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
        match test_sftp_server::spawn(root.into(), port, options).await {
            Ok(addr) => {
                println!("listening {addr}");
                std::future::pending::<()>().await;
            }
            Err(err) => {
                eprintln!("起不来：{err}");
                std::process::exit(1);
            }
        }
    });
}
