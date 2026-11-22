//! 调试用的进程内假 Telnet 设备：只在 debug 构建里由 `ZSSH_FAKE_TELNET_PORT=<端口>` 起一个回环监听，
//! 窗口级场景（`scripts/native-shot.ps1`）用它验证"新建 Telnet 主机 → 连接 → 标签状态"，不依赖真设备。
//!
//! 行为像一台最简单的网络设备：连上先协商（服务器回显 + 抑制 GA）、打横幅、问 `login:` 和
//! `Password:`，登录后给一个 `device#` 提示符，每行命令回一句 `你输入了：…`。输入的 IAC 协商字节
//! 全部吞掉。接受任意用户名 / 密码——只在本机回环上，不是任何真实服务。

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

const IAC: u8 = 255;
const WILL: u8 = 251;
const ECHO: u8 = 1;
const SGA: u8 = 3;

/// 读 `ZSSH_FAKE_TELNET_PORT` 并起监听线程；变量没设或端口占用都只是不起，不影响启动。
pub fn spawn_from_env() {
    // ZSSH_FAKE_TELNET_MARKS=1：登录后的提示符带 OSC 133 输入行标记（A 提示符开始 / B 输入起点 / C 命令开始 / D 命令结束），
    // 让命令建议的窗口级场景有一个"提供标记的 shell"。没设就和以前完全一样。
    let marks =
        crate::dev_hooks::var("ZSSH_FAKE_TELNET_MARKS").is_some_and(|raw| raw.trim() == "1");
    let Some(port) = crate::dev_hooks::var("ZSSH_FAKE_TELNET_PORT")
        .and_then(|raw| raw.trim().parse::<u16>().ok())
    else {
        return;
    };
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(%err, port, "假 Telnet 设备起不来");
            return;
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = serve_with(stream, marks);
            });
        }
    });
}

#[derive(PartialEq)]
enum Stage {
    User,
    Password,
    Shell,
}

#[cfg(test)]
pub(crate) fn serve(stream: TcpStream) -> std::io::Result<()> {
    serve_with(stream, false)
}

/// `marks` 为真时，shell 阶段的提示符和命令输出带 OSC 133 标记（见 [`spawn_from_env`]）。
pub(crate) fn serve_with(mut stream: TcpStream, marks: bool) -> std::io::Result<()> {
    stream.write_all(&[IAC, WILL, ECHO, IAC, WILL, SGA])?;
    stream.write_all("ZeeTerm 假 Telnet 设备（调试用）\r\nlogin: ".as_bytes())?;
    let mut stage = Stage::User;
    let mut line = Vec::<u8>::new();
    let mut buf = [0u8; 256];
    // 解析输入里的 IAC 序列：状态 0 = 普通，1 = 刚见到 IAC，2 = 在 WILL/WONT/DO/DONT 后等选项字节。
    let mut iac = 0u8;
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        for &byte in &buf[..n] {
            match iac {
                1 => {
                    iac = if (251..=254).contains(&byte) { 2 } else { 0 };
                    continue;
                }
                2 => {
                    iac = 0;
                    continue;
                }
                _ if byte == IAC => {
                    iac = 1;
                    continue;
                }
                _ => {}
            }
            match byte {
                b'\r' => {
                    stream.write_all(b"\r\n")?;
                    let text = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    match stage {
                        Stage::User => {
                            stage = Stage::Password;
                            stream.write_all(b"Password: ")?;
                        }
                        Stage::Password => {
                            stage = Stage::Shell;
                            if marks {
                                stream.write_all(
                                    b"\r\nlogin ok\r\n\x1b]133;A\x07device# \x1b]133;B\x07",
                                )?;
                            } else {
                                stream.write_all(b"\r\nlogin ok\r\ndevice# ")?;
                            }
                        }
                        Stage::Shell => {
                            if text.trim() == "quit" {
                                stream.write_all("再见\r\n".as_bytes())?;
                                return Ok(());
                            }
                            if marks {
                                stream.write_all(
                                    format!(
                                        "\x1b]133;C\x07你输入了：{text}\r\n\x1b]133;D;0\x07\x1b]133;A\x07device# \x1b]133;B\x07"
                                    )
                                    .as_bytes(),
                                )?;
                            } else {
                                stream.write_all(
                                    format!("你输入了：{text}\r\ndevice# ").as_bytes(),
                                )?;
                            }
                        }
                    }
                }
                b'\n' | 0 => {}
                0x7f | 0x08 => {
                    if line.pop().is_some() && stage != Stage::Password {
                        stream.write_all(b"\x08 \x08")?;
                    }
                }
                other => {
                    line.push(other);
                    // 服务器回显（密码不回显）。
                    if stage != Stage::Password {
                        stream.write_all(&[other])?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_fake_device_walks_through_login_and_echoes_commands() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = serve(stream);
            }
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client
            .write_all(b"admin\rsecret\rshow version\rquit\r")
            .unwrap();
        let mut text = String::new();
        let mut reader = BufReader::new(client);
        let mut raw = Vec::new();
        while reader.read_until(b'\n', &mut raw).unwrap_or(0) > 0 {}
        text.push_str(&String::from_utf8_lossy(&raw));
        assert!(text.contains("login ok"), "{text}");
        assert!(text.contains("你输入了：show version"), "{text}");
        assert!(!text.contains("secret"), "密码不回显：{text}");
        assert!(
            !text.contains("\x1b]133"),
            "默认不带 OSC 133 标记：{text:?}"
        );
    }

    #[test]
    fn with_marks_the_prompt_and_each_command_carry_osc_133() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = serve_with(stream, true);
            }
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(b"admin\rsecret\rshow\rquit\r").unwrap();
        let mut reader = BufReader::new(client);
        let mut raw = Vec::new();
        while reader.read_until(b'\n', &mut raw).unwrap_or(0) > 0 {}
        let text = String::from_utf8_lossy(&raw).into_owned();
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("缺 {needle:?}: {text:?}"))
        };
        // 登录后第一个提示符：A 在提示符前，B 在提示符后；命令：C 在输出前，D 在输出后，然后新的 A / B。
        assert!(at("\x1b]133;A\x07device# \x1b]133;B\x07") < at("\x1b]133;C\x07"));
        assert!(at("\x1b]133;C\x07") < at("你输入了：show"));
        assert!(at("你输入了：show") < at("\x1b]133;D;0\x07"));
        assert_eq!(text.matches("\x1b]133;B\x07").count(), 2, "{text:?}");
    }
}
