//! 网络连接列表（V2-U12）：连测试服务器，在已有连接上真跑一次采集脚本（只读命令），
//! 核对至少能解析出自己这条 SSH 连接。
//!
//! 在完整联网档里跑（`scripts/live-tests.ps1`）；服务器不是 Linux 时打印原因并跳过。

mod common;

use zssh_core::ssh::{collect_connections, ConnState, ConnectionsError};

use common::{full_live, serial, TempKnownHosts, TestServer};

#[tokio::test]
async fn the_collected_listing_contains_our_own_ssh_connection() {
    if !full_live("the_collected_listing_contains_our_own_ssh_connection") {
        return;
    }
    let _serial = serial().await;
    let server = match TestServer::from_env() {
        Ok(server) => server,
        Err(reason) => {
            println!("跳过 connections_live：{reason}");
            return;
        }
    };
    let known_hosts = TempKnownHosts::new("connections-live");
    let connection = server.connect(&known_hosts).await.expect("连接测试服务器");
    match collect_connections(&connection).await {
        Ok(report) => {
            println!(
                "来源 {}，{} 条，截断 {}",
                report.source.label(),
                report.rows.len(),
                report.truncated
            );
            assert!(
                report
                    .rows
                    .iter()
                    .any(|row| row.state == ConnState::Established),
                "至少有一条已建立的连接（就是这条 SSH）"
            );
        }
        Err(ConnectionsError::Unsupported) => {
            println!("跳过断言：服务器上没有 ss / netstat / /proc/net");
        }
        Err(other) => panic!("采集失败：{other}"),
    }
}
