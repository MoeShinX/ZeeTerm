//! 连上主机之后在后台探一次远端系统，结果存进库（`host_os` 表），主机树里的图标据此换成发行版标志。
//!
//! 什么时候跑：只在**新建**了一条已认证的 SSH 连接之后（`ssh_backend::dial`），池里复用的连接不再探
//! （它建的时候已经探过）；Telnet / 串口、快速连接（没有主机记录）不探。探测命令是只读的
//! （`cat /etc/os-release || uname -s`），走连接上的一条 exec 通道，不碰终端的 shell；
//! 失败、超时、认不出来都当没发生，界面继续用通用图标。
//!
//! 怎么存：只在结果和库里记的不一样时才写（`Database::host_os_set` 自己判断）。表只在本机，
//! 不进配置同步和备份（下次连上会重新探测），主机删除时由外键级联清掉。界面不需要通知：
//! 主机库每 2 秒重读一次，下一轮就会看到。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zssh_core::ssh::connection::Connection;
use zssh_core::ssh::os_probe::{OS_PROBE_COMMAND, classify};
use zssh_core::store::Database;

/// 探测命令最多等多久；超时当没探到。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// 输出上限：os-release 通常不到 1 KiB。
const PROBE_MAX_BYTES: usize = 16 * 1024;

/// 一次探测的结局（给日志和测试看）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// 认出了系统，并且和库里记的不一样（新写的或改了）。
    Changed(&'static str),
    /// 认出了系统，和库里记的一样，没有写库。
    Unchanged(&'static str),
    /// 输出认不出来（不是 Linux / BSD / macOS，或命令被拒）：库里原来的记录不动。
    Unknown,
}

/// 探测命令的输出 → 写库。写失败只返回原因，不影响连接。
pub fn record_output(db: &Database, host_id: i64, output: &str) -> Result<Recorded, String> {
    let Some(os) = classify(output) else {
        return Ok(Recorded::Unknown);
    };
    let changed = db.host_os_set(host_id, os).map_err(|err| err.to_string())?;
    Ok(if changed {
        Recorded::Changed(os)
    } else {
        Recorded::Unchanged(os)
    })
}

/// 在已认证的连接上跑探测命令；失败、超时都是 `None`。
pub async fn probe_output(connection: &Connection) -> Option<String> {
    match connection
        .exec_capture(OS_PROBE_COMMAND, PROBE_TIMEOUT, PROBE_MAX_BYTES)
        .await
    {
        Ok(capture) if !capture.timed_out => Some(capture.stdout),
        Ok(_) => None,
        Err(err) => {
            tracing::debug!(%err, "探测系统类型失败");
            None
        }
    }
}

/// 探一次并写库（库路径给进来，库句柄不是 `Send`，不能跨 `await`，所以探完再开）。
pub async fn probe_and_record(
    connection: &Connection,
    db_path: &Path,
    host_id: i64,
) -> Option<Recorded> {
    let output = probe_output(connection).await?;
    let db = match Database::open(db_path) {
        Ok(db) => db,
        Err(err) => {
            tracing::warn!(%err, "记录系统类型：打开主机库失败");
            return None;
        }
    };
    match record_output(&db, host_id, &output) {
        Ok(outcome) => Some(outcome),
        Err(err) => {
            tracing::warn!(%err, "记录系统类型失败");
            None
        }
    }
}

/// 后台探一次（不等结果）。
pub fn spawn_probe(host_id: i64, connection: Arc<Connection>) {
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    runtime.spawn(async move {
        let db_path = crate::settings::data_dir().join("zssh.db");
        let _ = probe_and_record(&connection, &db_path, host_id).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ssh_server;

    fn save_host(db: &Database) -> i64 {
        use zssh_core::model::{AuthMethod, HostDraft, ProxyMode};
        db.host_save(&HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: "web-1".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        })
        .expect("存主机")
        .id
    }

    fn db_with_host() -> (Database, i64) {
        let db = Database::open_in_memory().expect("内存库");
        let host_id = save_host(&db);
        (db, host_id)
    }

    #[test]
    fn writes_only_when_the_result_changes() {
        let (db, id) = db_with_host();
        let debian = "ID=debian\nNAME=\"Debian GNU/Linux\"\n";
        assert_eq!(
            record_output(&db, id, debian),
            Ok(Recorded::Changed("debian"))
        );
        assert_eq!(
            record_output(&db, id, debian),
            Ok(Recorded::Unchanged("debian"))
        );
        // 重装成别的系统：改写。
        assert_eq!(
            record_output(&db, id, "ID=ubuntu\n"),
            Ok(Recorded::Changed("ubuntu"))
        );
        assert_eq!(
            db.host_os_list().expect("读").get(&id).map(String::as_str),
            Some("ubuntu")
        );
    }

    #[test]
    fn unrecognised_output_leaves_the_old_record_alone() {
        let (db, id) = db_with_host();
        record_output(&db, id, "ID=alpine\n").expect("写");
        for garbage in [
            "",
            "SunOS\n",
            "'cat' is not recognized as an internal or external command",
        ] {
            assert_eq!(record_output(&db, id, garbage), Ok(Recorded::Unknown));
        }
        assert_eq!(
            db.host_os_list().expect("读").get(&id).map(String::as_str),
            Some("alpine")
        );
    }

    #[test]
    fn every_sample_distro_lands_on_an_id_that_has_an_icon() {
        let samples = [
            ("ID=ubuntu\nID_LIKE=debian\n", "ubuntu"),
            ("ID=debian\n", "debian"),
            ("ID=\"centos\"\nID_LIKE=\"rhel fedora\"\n", "centos"),
            ("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n", "rocky"),
            ("ID=fedora\n", "fedora"),
            ("ID=arch\n", "arch"),
            ("ID=alpine\n", "alpine"),
            ("ID=\"amzn\"\nID_LIKE=\"centos rhel fedora\"\n", "amazon"),
            (
                "ID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\n",
                "opensuse",
            ),
            ("ID=\"rhel\"\nID_LIKE=\"fedora\"\n", "redhat"),
            ("ID=\"ol\"\nID_LIKE=\"fedora\"\n", "oracle"),
            ("ID=kali\nID_LIKE=debian\n", "kali"),
            ("ID=\"almalinux\"\n", "almalinux"),
            ("ID=\"alinux\"\nNAME=\"Alibaba Cloud Linux\"\n", "alinux"),
            ("ID=openEuler\n", "openeuler"),
            ("FreeBSD\n", "freebsd"),
            ("Darwin\n", "macos"),
            ("ID=nixos\nNAME=NixOS\n", "linux"),
            ("Linux\n", "linux"),
        ];
        for (sample, want) in samples {
            let got = classify(sample).unwrap_or_else(|| panic!("认不出 {sample:?}"));
            assert_eq!(got, want, "{sample:?}");
            assert!(crate::os_icons::is_known(got), "{got} 没有图标");
        }
    }

    /// 端到端：进程内迷你 SSH 服务器应答探测命令，连上之后探一次，库里写了结果；
    /// 再探一次内容相同，不重复写。
    #[test]
    fn probing_a_live_connection_writes_the_result_once() {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let dir = std::env::temp_dir().join(format!("zssh-u20-os-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let db_path = dir.join("zssh.db");
        let host_id = {
            let db = Database::open(&db_path).expect("开库");
            save_host(&db)
        };
        let addr = runtime.block_on(test_ssh_server::spawn_answering_exec(
            "NAME=\"Debian GNU/Linux\"\nID=debian\n",
        ));
        let connection = runtime.block_on(crate::test_ssh_server::connect_trusting(addr, &dir));
        let first = runtime.block_on(probe_and_record(&connection, &db_path, host_id));
        let second = runtime.block_on(probe_and_record(&connection, &db_path, host_id));
        let stored = Database::open(&db_path)
            .expect("重新开库")
            .host_os_list()
            .expect("读");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first, Some(Recorded::Changed("debian")));
        assert_eq!(second, Some(Recorded::Unchanged("debian")));
        assert_eq!(stored.get(&host_id).map(String::as_str), Some("debian"));
    }

    /// 服务器不让跑 exec（有些设备）：探测当没发生，不写库。
    #[test]
    fn a_server_that_refuses_exec_leaves_the_host_unprobed() {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let dir = std::env::temp_dir().join(format!("zssh-u20-os-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let db_path = dir.join("zssh.db");
        let host_id = {
            let db = Database::open(&db_path).expect("开库");
            save_host(&db)
        };
        let addr = runtime.block_on(test_ssh_server::spawn());
        let connection = runtime.block_on(crate::test_ssh_server::connect_trusting(addr, &dir));
        let outcome = runtime.block_on(probe_and_record(&connection, &db_path, host_id));
        let stored = Database::open(&db_path)
            .expect("重新开库")
            .host_os_list()
            .expect("读");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(outcome, Some(Recorded::Changed("debian")));
        assert!(stored.is_empty(), "{stored:?}");
    }
}
