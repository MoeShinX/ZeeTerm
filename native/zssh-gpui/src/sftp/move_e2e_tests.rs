//! 「移动到…」的端到端测试（V2-U23）：真实的 SFTP 客户端（`SftpChannel`）连进程内 SFTP 服务器夹具
//! （`test_sftp_server`，根目录是临时目录），用状态机 [`MoveJob`] 和真正动文件的 [`remote_step`] 跑完整流程。
//! 不连任何真实服务器。服务器分两种：支持 `posix-rename` 的，和不支持的（覆盖走客户端的“备用名”三步）。

use std::sync::Arc;

use zssh_core::sftp::SftpChannel;
use zssh_core::ssh::connection::Connection;

use super::menu::Side;
use super::model::Entry;
use super::move_to::{
    Choice, MoveAsk, MoveJob, Next, StepResult, Tally, remote_folders, remote_step,
};
use crate::test_sftp_server::{Options, TempRoot, spawn};
use crate::test_ssh_server::connect_trusting;

struct World {
    root: TempRoot,
    channel: SftpChannel,
    // 通道靠这条连接活着。
    _connection: Arc<Connection>,
    _known_hosts: TempRoot,
}

async fn world(posix_rename: bool) -> World {
    let root = TempRoot::new("move-root");
    let known_hosts = TempRoot::new("move-kh");
    let addr = spawn(root.path().into(), 0, Options { posix_rename })
        .await
        .expect("起夹具服务器");
    let connection = Arc::new(connect_trusting(addr, known_hosts.path()).await);
    let channel = SftpChannel::open(&connection).await.expect("开 SFTP 通道");
    World {
        root,
        channel,
        _connection: connection,
        _known_hosts: known_hosts,
    }
}

/// 列出远端某目录里指定名字的条目（和界面里选中条目拿到的是同一种 `Entry`）。
async fn entries(channel: &SftpChannel, dir: &str, names: &[&str]) -> Vec<Entry> {
    let listing = channel.read_dir(dir).await.expect("列目录");
    names
        .iter()
        .map(|name| {
            let found = listing
                .iter()
                .find(|entry| entry.name == *name)
                .unwrap_or_else(|| panic!("{dir} 里没有 {name}"));
            Entry::from_remote(found)
        })
        .collect()
}

/// 跑完一个任务；同名询问由 `answer` 回答。
async fn drive(
    channel: &SftpChannel,
    job: &mut MoveJob,
    answer: &mut dyn FnMut(&MoveAsk) -> (Choice, bool),
) -> Tally {
    let free = |_: &Entry| None;
    let mut next = job.advance(&free);
    loop {
        next = match next {
            Next::Attempt {
                entry,
                target,
                overwrite,
            } => {
                let result = remote_step(channel, &entry.path, &target, overwrite).await;
                job.on_step(result, &free)
            }
            Next::Ask(ask) => {
                let (choice, rest) = answer(&ask);
                job.answer(choice, rest, &free)
            }
            Next::Done(tally) => return tally,
        };
    }
}

fn new_job(dest: &str, entries: Vec<Entry>) -> MoveJob {
    MoveJob::new(1, Side::Remote, dest.to_owned(), entries)
}

#[tokio::test(flavor = "multi_thread")]
async fn files_and_folders_with_awkward_names_move_whole_on_both_server_kinds() {
    for posix in [true, false] {
        let world = world(posix).await;
        world.root.mkdir("目标 文件夹");
        world.root.write("报告 最终 'v2' $x;.txt", b"body");
        world.root.write("tree/inner/deeper/x.txt", b"x");
        let picked = entries(&world.channel, "/", &["报告 最终 'v2' $x;.txt", "tree"]).await;
        let mut job = new_job("/目标 文件夹", picked);
        let tally = drive(&world.channel, &mut job, &mut |_| unreachable!("没有同名")).await;
        assert_eq!(
            tally.moved,
            ["报告 最终 'v2' $x;.txt", "tree"],
            "posix={posix}"
        );
        assert!(!tally.has_problems());
        assert!(!world.root.exists("报告 最终 'v2' $x;.txt"));
        assert!(!world.root.exists("tree"));
        assert_eq!(
            world
                .root
                .read("目标 文件夹/报告 最终 'v2' $x;.txt")
                .as_deref(),
            Some(&b"body"[..])
        );
        assert_eq!(
            world
                .root
                .read("目标 文件夹/tree/inner/deeper/x.txt")
                .as_deref(),
            Some(&b"x"[..]),
            "整棵树一起搬过去"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clash_can_be_replaced_or_skipped_per_item_and_folders_only_skip() {
    for posix in [true, false] {
        let world = world(posix).await;
        world.root.write("dest/a.txt", b"old-a");
        world.root.write("dest/b.txt", b"old-b");
        world.root.write("dest/dir/keep.txt", b"keep");
        world.root.write("a.txt", b"new-a");
        world.root.write("b.txt", b"new-b");
        world.root.write("dir/other.txt", b"other");
        let picked = entries(&world.channel, "/", &["a.txt", "b.txt", "dir"]).await;
        let mut job = new_job("/dest", picked);
        let mut asked: Vec<(String, bool)> = Vec::new();
        let tally = drive(&world.channel, &mut job, &mut |ask| {
            asked.push((ask.name.clone(), ask.can_replace));
            match ask.name.as_str() {
                "a.txt" => (Choice::Replace, false),
                _ => (Choice::Skip, false),
            }
        })
        .await;
        assert_eq!(
            asked,
            [
                ("a.txt".to_owned(), true),
                ("b.txt".to_owned(), true),
                ("dir".to_owned(), false)
            ],
            "posix={posix}"
        );
        assert_eq!(tally.moved, ["a.txt"]);
        assert_eq!(tally.skipped.len(), 2);
        // 替换了 a.txt：旧内容没了，源没了，也没有留下备用文件。
        assert_eq!(
            world.root.read("dest/a.txt").as_deref(),
            Some(&b"new-a"[..])
        );
        assert!(!world.root.exists("a.txt"));
        let leftovers: Vec<String> = std::fs::read_dir(world.root.path().join("dest"))
            .expect("列 dest")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("zssh-old"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下备用文件：{leftovers:?}");
        // 跳过的两个：源和目标原样不动，文件夹没有被合并。
        assert_eq!(world.root.read("b.txt").as_deref(), Some(&b"new-b"[..]));
        assert_eq!(
            world.root.read("dest/b.txt").as_deref(),
            Some(&b"old-b"[..])
        );
        assert!(world.root.exists("dir/other.txt"));
        assert!(!world.root.exists("dest/dir/other.txt"));
        assert!(world.root.exists("dest/dir/keep.txt"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_to_the_rest_replaces_every_later_file_clash_without_asking_again() {
    let world = world(true).await;
    for name in ["a", "b", "c"] {
        world.root.write(&format!("dest/{name}.txt"), b"old");
        world
            .root
            .write(&format!("{name}.txt"), format!("new-{name}").as_bytes());
    }
    let picked = entries(&world.channel, "/", &["a.txt", "b.txt", "c.txt"]).await;
    let mut job = new_job("/dest", picked);
    let mut questions = 0;
    let tally = drive(&world.channel, &mut job, &mut |ask| {
        questions += 1;
        assert_eq!(ask.remaining, 2);
        (Choice::Replace, true)
    })
    .await;
    assert_eq!(questions, 1, "勾了“都这样做”只问一次");
    assert_eq!(tally.moved, ["a.txt", "b.txt", "c.txt"]);
    for name in ["a", "b", "c"] {
        assert_eq!(
            world.root.read(&format!("dest/{name}.txt")).as_deref(),
            Some(format!("new-{name}").as_bytes())
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_failure_does_not_stop_the_rest_and_nothing_is_created_for_a_missing_target() {
    let world = world(true).await;
    world.root.mkdir("dest");
    world.root.write("a.txt", b"a");
    world.root.write("b.txt", b"b");
    let mut picked = entries(&world.channel, "/", &["a.txt", "b.txt"]).await;
    // a.txt 在选中之后被别人删了：这一项失败，b.txt 照常移动。
    std::fs::remove_file(world.root.path().join("a.txt")).expect("删 a");
    let mut job = new_job("/dest", picked.clone());
    let tally = drive(&world.channel, &mut job, &mut |_| unreachable!()).await;
    assert_eq!(tally.moved, ["b.txt"]);
    assert_eq!(tally.failed.len(), 1);
    assert_eq!(tally.failed[0].name, "a.txt");
    assert!(!tally.failed[0].reason.is_empty());
    assert!(
        tally
            .summary("dest")
            .starts_with("已移动 b.txt 到「dest」，失败 1 项：a.txt（"),
        "{}",
        tally.summary("dest")
    );

    // 目标文件夹不存在：每一项都失败，服务器上不会冒出这个文件夹。
    world.root.write("c.txt", b"c");
    picked = entries(&world.channel, "/", &["c.txt"]).await;
    let mut job = new_job("/nowhere", picked);
    let tally = drive(&world.channel, &mut job, &mut |_| unreachable!()).await;
    assert!(tally.moved.is_empty());
    assert_eq!(tally.failed.len(), 1);
    assert!(world.root.exists("c.txt"));
    assert!(!world.root.exists("nowhere"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_raw_step_reports_the_kind_of_the_clashing_target() {
    let world = world(true).await;
    world.root.write("src.txt", b"s");
    world.root.write("file-target.txt", b"t");
    world.root.mkdir("dir-target");
    assert_eq!(
        remote_step(&world.channel, "/src.txt", "/file-target.txt", false).await,
        StepResult::Exists {
            target_is_dir: false
        }
    );
    assert_eq!(
        remote_step(&world.channel, "/src.txt", "/dir-target", false).await,
        StepResult::Exists {
            target_is_dir: true
        }
    );
    // 都没动。
    assert!(world.root.exists("src.txt"));
    assert_eq!(
        remote_step(&world.channel, "/src.txt", "/moved.txt", false).await,
        StepResult::Moved
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_lists_only_the_sub_folders_of_a_remote_directory() {
    let world = world(true).await;
    world.root.mkdir("work/zeta");
    world.root.mkdir("work/Alpha");
    world.root.mkdir("work/.hidden-dir");
    world.root.write("work/readme.txt", b"r");
    let folders = remote_folders(&world.channel, "/work")
        .await
        .expect("列子文件夹");
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"Alpha") && names.contains(&"zeta"));
    assert!(names.contains(&".hidden-dir"));
    assert!(!names.contains(&"readme.txt"));
    let alpha = names.iter().position(|n| *n == "Alpha").expect("Alpha");
    let zeta = names.iter().position(|n| *n == "zeta").expect("zeta");
    assert!(alpha < zeta, "按名字排序：{names:?}");
    assert!(folders.iter().all(|f| f.path.starts_with("/work/")));
    assert!(remote_folders(&world.channel, "/missing").await.is_err());
}
