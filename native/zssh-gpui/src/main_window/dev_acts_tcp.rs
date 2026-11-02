//! 调试驱动用的本机 TCP 探测：转发验证时从本机端口发一个 HTTP 请求，看回应里有没有预期的文字
//! （或者端口确实连不上）。探测在后台线程里做，界面线程只轮询结果，不会卡在网络上。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::dev_script::Outcome;

enum Slot {
    Running,
    Done(Result<(), String>),
}

fn slots() -> &'static Mutex<HashMap<String, Slot>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 第一次调用起后台线程；之后每次调用看结果，没好返回 `Pending`，好了返回 `Done` / `Failed`。
pub fn poll(key: &str, work: impl FnOnce() -> Result<(), String> + Send + 'static) -> Outcome {
    let mut map = slots().lock().unwrap_or_else(|p| p.into_inner());
    match map.get(key) {
        None => {
            map.insert(key.to_owned(), Slot::Running);
            let key = key.to_owned();
            std::thread::spawn(move || {
                let result = work();
                slots()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, Slot::Done(result));
            });
            Outcome::Pending
        }
        Some(Slot::Running) => Outcome::Pending,
        Some(Slot::Done(result)) => {
            let outcome = match result {
                Ok(()) => Outcome::Done,
                Err(reason) => Outcome::Failed(reason.clone()),
            };
            // 同一步下次再来（比如重跑）要重新探测。
            map.remove(key);
            outcome
        }
    }
}

/// 连 127.0.0.1:端口。`refused_wanted` 为真：要求连不上；否则要求发 `GET /` 后回应里有 `expect`。
pub fn probe(port: u16, expect: &str, refused_wanted: bool) -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Err(_) if refused_wanted => Ok(()),
        Err(err) => Err(format!("连不上本机端口：{err}")),
        Ok(_) if refused_wanted => Err("这个端口应该已经关了，却连上了".to_owned()),
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
            stream
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .map_err(|err| format!("发请求失败：{err}"))?;
            let mut body = Vec::new();
            let _ = stream.read_to_end(&mut body);
            if String::from_utf8_lossy(&body).contains(expect) {
                Ok(())
            } else {
                Err("回应里没有预期的文字".to_owned())
            }
        }
    }
}
