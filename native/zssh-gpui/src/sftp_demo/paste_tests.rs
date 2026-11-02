// 粘贴的执行：本地复制（不联网）和真实服务器上的服务器端复制 / 中转复制（联网，完整档）。
// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zssh_core::sftp::SftpChannel;
use zssh_core::sftp::SftpError;
use zssh_core::sftp::fs;
use zssh_core::sftp::transfer::backend::{
    SftpChannelProvider, SftpEndpoints, TransferChannelProvider,
};
use zssh_core::sftp::transfer::{
    ConflictAction, Direction, TransferEngine, TransferEvent, TransferId, TransferOptions,
    TransferState,
};
use zssh_core::ssh::BoxFuture;

use super::SftpEvent;
use super::clip::{self, ClipItem, ClipSource, ItemState, Method};
use super::engine::{ConflictAutomation, UiConflictResolver};
use super::menu::Side;
use super::paste::{PasteCtx, PasteRequest, PastedJob, paste_local, run_paste};
use zssh_core::sftp::transfer::ConflictResolver;

/// 剪贴板是全局的：碰它的测试串行。
static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

fn runtime() -> &'static tokio::runtime::Runtime {
    crate::ssh_backend::runtime().expect("runtime")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("zssh-paste-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 没有远端的数据面：本地复制不会去要通道，要了就报错（顺便证明本地复制不碰网络）。
struct NoChannels;

impl TransferChannelProvider for NoChannels {
    fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        Box::pin(async { Err(SftpError::Remote("测试里没有远端".to_owned())) })
    }
}

type Events = tokio::sync::mpsc::UnboundedReceiver<TransferEvent>;

/// 等这些任务全部结束，返回每个任务的终态。
async fn finish(events: &mut Events, jobs: &[PastedJob]) -> Vec<(TransferId, TransferState)> {
    let mut pending: HashSet<TransferId> = jobs.iter().map(|job| job.id).collect();
    let mut done = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !pending.is_empty() {
        assert!(std::time::Instant::now() < deadline, "任务 60 秒内没结束");
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(100), events.recv()).await
        {
            let (TransferEvent::Progress(report) | TransferEvent::Finished(report)) = event;
            if report.state.is_terminal() && pending.remove(&report.id) {
                done.push((report.id, report.state));
            }
        }
    }
    done
}

struct Local {
    engine: Arc<TransferEngine>,
    events: Events,
    resolver: Arc<UiConflictResolver>,
    conflicts: Receiver<SftpEvent>,
}

fn local_setup(automation: Option<ConflictAutomation>) -> Local {
    let provider: Arc<dyn TransferChannelProvider> = Arc::new(NoChannels);
    let endpoints = Arc::new(SftpEndpoints::new(provider));
    let (sender, conflicts) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(sender, automation));
    let decision_maker: Arc<dyn ConflictResolver> = resolver.clone();
    let rt = runtime();
    let _guard = rt.enter();
    let (engine, events) =
        TransferEngine::new(endpoints, decision_maker, TransferOptions::default());
    Local {
        engine,
        events,
        resolver,
        conflicts,
    }
}

fn local_request(items: Vec<ClipItem>, target_dir: &Path) -> PasteRequest {
    PasteRequest {
        op: 1,
        force_relay: false,
        side: Side::Local,
        items,
        target_dir: target_dir.display().to_string(),
        source: ClipSource {
            view: 1,
            server_key: None,
            provider: None,
        },
        this_view: 1,
    }
}

fn file_item(path: &Path) -> ClipItem {
    ClipItem {
        path: path.display().to_string(),
        is_dir: false,
    }
}

fn dir_item(path: &Path) -> ClipItem {
    ClipItem {
        path: path.display().to_string(),
        is_dir: true,
    }
}

#[test]
fn local_paste_copies_files_and_folders_into_the_target() {
    let scratch = Scratch::new("copy");
    let src = scratch.path().join("src");
    let dst = scratch.path().join("dst");
    std::fs::create_dir_all(src.join("tree/inner")).expect("建源");
    std::fs::create_dir_all(&dst).expect("建目标");
    std::fs::write(src.join("a b 中文.txt"), "alpha").expect("写");
    std::fs::write(src.join("tree/x.txt"), "x").expect("写");
    std::fs::write(src.join("tree/inner/y.txt"), "y").expect("写");

    let mut local = local_setup(None);
    let request = local_request(
        vec![
            file_item(&src.join("a b 中文.txt")),
            dir_item(&src.join("tree")),
        ],
        &dst,
    );
    let rt = runtime();
    let (outcome, jobs) = rt
        .block_on(paste_local(&local.engine, &local.resolver, &request))
        .expect("粘贴");
    assert_eq!(outcome.method, Method::Local);
    assert_eq!(jobs.len(), 2);
    assert!(outcome.items.iter().all(|i| i.state == ItemState::Queued));
    assert!(jobs.iter().all(|j| j.direction == Direction::LocalCopy));
    let states = rt.block_on(finish(&mut local.events, &jobs));
    assert!(
        states.iter().all(|(_, s)| matches!(s, TransferState::Done)),
        "{states:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("a b 中文.txt")).unwrap_or_default(),
        "alpha"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("tree/inner/y.txt")).unwrap_or_default(),
        "y"
    );
    // 源原封不动。
    assert!(src.join("tree/x.txt").is_file());
}

#[test]
fn pasting_into_the_source_folder_renames_instead_of_overwriting_itself() {
    let scratch = Scratch::new("self");
    let file = scratch.path().join("note.txt");
    std::fs::write(&file, "keep").expect("写");

    // 自动化回答「重命名」：不指定名字，走「名字 - 副本」。
    let mut local = local_setup(Some(ConflictAutomation::Rename));
    let request = local_request(vec![file_item(&file)], scratch.path());
    let rt = runtime();
    let (outcome, jobs) = rt
        .block_on(paste_local(&local.engine, &local.resolver, &request))
        .expect("粘贴");
    assert_eq!(outcome.items[0].state, ItemState::Queued);
    rt.block_on(finish(&mut local.events, &jobs));
    assert_eq!(std::fs::read_to_string(&file).unwrap_or_default(), "keep");
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("note - 副本.txt")).unwrap_or_default(),
        "keep"
    );

    // 再粘一次：副本名顺延成「 - 副本 2」。
    let (_, jobs) = rt
        .block_on(paste_local(&local.engine, &local.resolver, &request))
        .expect("再粘贴");
    rt.block_on(finish(&mut local.events, &jobs));
    assert!(scratch.path().join("note - 副本 2.txt").is_file());
}

#[test]
fn pasting_onto_itself_can_skip_but_never_overwrite() {
    let scratch = Scratch::new("self2");
    let file = scratch.path().join("n.txt");
    std::fs::write(&file, "orig").expect("写");
    let rt = runtime();

    let local = local_setup(Some(ConflictAutomation::Skip));
    let (outcome, jobs) = rt
        .block_on(paste_local(
            &local.engine,
            &local.resolver,
            &local_request(vec![file_item(&file)], scratch.path()),
        ))
        .expect("粘贴");
    assert!(jobs.is_empty());
    assert_eq!(outcome.items[0].state, ItemState::Skipped);

    let local = local_setup(Some(ConflictAutomation::Overwrite));
    let (outcome, jobs) = rt
        .block_on(paste_local(
            &local.engine,
            &local.resolver,
            &local_request(vec![file_item(&file)], scratch.path()),
        ))
        .expect("粘贴");
    assert!(jobs.is_empty());
    assert_eq!(outcome.items[0].state, ItemState::Failed);
    assert!(
        outcome.items[0]
            .message
            .as_deref()
            .unwrap_or("")
            .contains("覆盖自己"),
        "{:?}",
        outcome.items[0].message
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap_or_default(), "orig");
}

#[test]
fn a_folder_cannot_be_pasted_into_itself() {
    let scratch = Scratch::new("into-self");
    let tree = scratch.path().join("tree");
    std::fs::create_dir_all(tree.join("sub")).expect("建");
    let local = local_setup(None);
    let rt = runtime();
    let error = rt
        .block_on(paste_local(
            &local.engine,
            &local.resolver,
            &local_request(vec![dir_item(&tree)], &tree.join("sub")),
        ))
        .err()
        .unwrap_or_default();
    assert!(error.contains("不能把文件夹「tree」"), "{error}");
    assert!(!tree.join("sub/tree").exists());
}

#[test]
fn same_names_in_another_folder_are_asked_per_file_by_the_engine() {
    let scratch = Scratch::new("conflict");
    let src = scratch.path().join("src");
    let dst = scratch.path().join("dst");
    std::fs::create_dir_all(&src).expect("建");
    std::fs::create_dir_all(&dst).expect("建");
    std::fs::write(src.join("same.txt"), "new").expect("写");
    std::fs::write(dst.join("same.txt"), "old").expect("写");
    let rt = runtime();

    // 跳过：目标保持旧内容。
    let mut local = local_setup(Some(ConflictAutomation::Skip));
    let (_, jobs) = rt
        .block_on(paste_local(
            &local.engine,
            &local.resolver,
            &local_request(vec![file_item(&src.join("same.txt"))], &dst),
        ))
        .expect("粘贴");
    rt.block_on(finish(&mut local.events, &jobs));
    assert_eq!(
        std::fs::read_to_string(dst.join("same.txt")).unwrap_or_default(),
        "old"
    );

    // 覆盖：换成新内容。
    let mut local = local_setup(Some(ConflictAutomation::Overwrite));
    let (_, jobs) = rt
        .block_on(paste_local(
            &local.engine,
            &local.resolver,
            &local_request(vec![file_item(&src.join("same.txt"))], &dst),
        ))
        .expect("粘贴");
    rt.block_on(finish(&mut local.events, &jobs));
    assert_eq!(
        std::fs::read_to_string(dst.join("same.txt")).unwrap_or_default(),
        "new"
    );
    assert!(
        local.conflicts.try_recv().is_err(),
        "有自动化回答时不弹面板"
    );
}

#[test]
fn paste_rules_follow_the_clipboard_side() {
    let _lock = CLIPBOARD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clip::clear();
    assert_eq!(
        super::copy::paste_block_reason(Side::Remote, "/srv").as_deref(),
        Some("剪贴板是空的")
    );
    let source = ClipSource {
        view: 3,
        server_key: Some("u@h:22".to_owned()),
        provider: None,
    };
    clip::set(clip::ClipContent {
        side: Side::Remote,
        items: vec![ClipItem {
            path: "/srv/a".to_owned(),
            is_dir: false,
        }],
        source,
    });
    assert_eq!(super::copy::paste_block_reason(Side::Remote, "/srv"), None);
    // 远端复制的东西不能粘贴到本地栏。
    assert_eq!(
        super::copy::paste_block_reason(Side::Local, r"C:\work").as_deref(),
        Some("远端和本地之间请用上传 / 下载")
    );
    // 目录还没读出来。
    assert_eq!(
        super::copy::paste_block_reason(Side::Remote, "").as_deref(),
        Some("先进入一个目录")
    );
    clip::clear();
}

// ---- 联网：真实服务器 ----

fn remote_ctx(
    live: &crate::live_support::LiveSftp,
    automation: Option<ConflictAutomation>,
) -> (PasteCtx, Events, Arc<SftpChannelProvider>) {
    let (ctx, events, provider, _conflicts) = remote_ctx_with_prompts(live, automation);
    (ctx, events, provider)
}

/// 同 [`remote_ctx`]，多返回一个提问通道：没有自动化时，界面该回答的提问从这里出来。
fn remote_ctx_with_prompts(
    live: &crate::live_support::LiveSftp,
    automation: Option<ConflictAutomation>,
) -> (
    PasteCtx,
    Events,
    Arc<SftpChannelProvider>,
    Receiver<SftpEvent>,
) {
    let provider = Arc::new(SftpChannelProvider::new(Arc::clone(&live.connection), None));
    let endpoints = Arc::new(SftpEndpoints::new(provider.clone()));
    let (sender, conflicts) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(sender.clone(), automation));
    let decision_maker: Arc<dyn ConflictResolver> = resolver.clone();
    let rt = runtime();
    let _guard = rt.enter();
    let (engine, events) = TransferEngine::new(
        endpoints.clone(),
        decision_maker,
        TransferOptions::default(),
    );
    (
        PasteCtx {
            browse: Arc::clone(&live.channel),
            connection: Arc::clone(&live.connection),
            engine,
            endpoints,
            resolver,
            cancel: Arc::new(AtomicBool::new(false)),
            sender,
            shell_kind: Arc::new(Mutex::new(None)),
            no_copy_data: false,
        },
        events,
        provider,
        conflicts,
    )
}

async fn put(channel: &SftpChannel, path: &str, data: &[u8]) {
    fs::write_bytes(channel, path, data, u64::MAX)
        .await
        .expect("写远端文件");
}

async fn get(channel: &SftpChannel, path: &str) -> Vec<u8> {
    fs::read_bytes(channel, path, 1 << 20)
        .await
        .expect("读远端文件")
}

/// 在测试服务器上建一棵小树：`tree/{a.txt, sub/b.txt, 中 文 c.txt}`。
async fn make_tree(channel: &SftpChannel, root: &str) {
    channel
        .create_dir(&format!("{root}/tree"))
        .await
        .expect("建树");
    channel
        .create_dir(&format!("{root}/tree/sub"))
        .await
        .expect("建子目录");
    put(channel, &format!("{root}/tree/a.txt"), b"alpha").await;
    put(channel, &format!("{root}/tree/sub/b.txt"), b"beta").await;
    put(
        channel,
        &format!("{root}/tree/中 文 c.txt"),
        "gamma".as_bytes(),
    )
    .await;
}

fn remote_request(
    ctx: &PasteCtx,
    items: Vec<ClipItem>,
    target_dir: &str,
    force_relay: bool,
) -> PasteRequest {
    PasteRequest {
        op: 1,
        force_relay,
        side: Side::Remote,
        items,
        target_dir: target_dir.to_owned(),
        source: ClipSource {
            view: 1,
            server_key: Some(ctx.browse.server_key().to_owned()),
            provider: None,
        },
        this_view: 1,
    }
}

/// 同一台主机：服务器端复制一个文件夹和一个文件，内容逐个核对；粘贴到自己所在的目录先问、起「 - 副本」名。
#[test]
fn live_same_host_copy_runs_on_the_server() {
    let Some(live) = crate::live_support::LiveSftp::connect("g08b-same") else {
        return;
    };
    let (ctx, _events, _provider) = remote_ctx(&live, Some(ConflictAutomation::Rename));
    let root = live.dir.clone();
    let rt = runtime();
    rt.block_on(async {
        make_tree(&live.channel, &root).await;
        live.channel
            .create_dir(&format!("{root}/dest"))
            .await
            .expect("建目标");
        put(&live.channel, &format!("{root}/solo.txt"), b"solo").await;

        let request = remote_request(
            &ctx,
            vec![
                ClipItem {
                    path: format!("{root}/tree"),
                    is_dir: true,
                },
                ClipItem {
                    path: format!("{root}/solo.txt"),
                    is_dir: false,
                },
            ],
            &format!("{root}/dest"),
            false,
        );
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("粘贴");
        assert!(jobs.is_empty(), "服务器端复制不进队列：{jobs:?}");
        println!("同一台主机的复制办法：{:?}", outcome.method);
        assert!(
            matches!(outcome.method, Method::CopyData | Method::ServerCp),
            "同一台主机应该在服务器上复制，实际 {:?}",
            outcome.method
        );
        assert!(
            outcome.items.iter().all(|i| i.state == ItemState::Ok),
            "{:?}",
            outcome.items
        );
        assert_eq!(
            get(&live.channel, &format!("{root}/dest/tree/a.txt")).await,
            b"alpha"
        );
        assert_eq!(
            get(&live.channel, &format!("{root}/dest/tree/sub/b.txt")).await,
            b"beta"
        );
        assert_eq!(
            get(&live.channel, &format!("{root}/dest/tree/中 文 c.txt")).await,
            "gamma".as_bytes()
        );
        assert_eq!(
            get(&live.channel, &format!("{root}/dest/solo.txt")).await,
            b"solo"
        );
        // 源原封不动。
        assert_eq!(
            get(&live.channel, &format!("{root}/tree/a.txt")).await,
            b"alpha"
        );

        // 粘贴到自己所在的目录：名字必然撞上自己，先问；自动化回答「重命名」→ 「solo - 副本.txt」。
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{root}/solo.txt"),
                is_dir: false,
            }],
            &root,
            false,
        );
        let (outcome, _) = run_paste(ctx.clone(), request)
            .await
            .expect("粘贴到自己的目录");
        assert_eq!(outcome.items[0].state, ItemState::Ok, "{:?}", outcome.items);
        assert_eq!(
            get(&live.channel, &format!("{root}/solo - 副本.txt")).await,
            b"solo"
        );
        assert_eq!(
            get(&live.channel, &format!("{root}/solo.txt")).await,
            b"solo"
        );

        // 文件夹不能粘贴进它自己。
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{root}/tree"),
                is_dir: true,
            }],
            &format!("{root}/tree/sub"),
            false,
        );
        let error = run_paste(ctx.clone(), request)
            .await
            .err()
            .unwrap_or_default();
        assert!(error.contains("不能把文件夹「tree」"), "{error}");
    });
}

/// 经本机中转：一次是同一台主机上用户选了"改为经本机中转"，一次是源在另一条连接上（用一个假的服务器
/// 身份把它当成另一台主机）。两种都进传输队列，数据经过本机。
#[test]
fn live_relay_copy_goes_through_the_local_queue_for_force_and_for_another_host() {
    let Some(source) = crate::live_support::LiveSftp::connect("g08b-src") else {
        return;
    };
    let Some(target) = crate::live_support::LiveSftp::connect("g08b-dst") else {
        return;
    };
    let (ctx, mut events, _target_provider) = remote_ctx(&target, None);
    let (_src_ctx, _src_events, src_provider) = remote_ctx(&source, None);
    let rt = runtime();
    rt.block_on(async {
        make_tree(&source.channel, &source.dir).await;
        target
            .channel
            .create_dir(&format!("{}/in", target.dir))
            .await
            .expect("建目标");

        // 另一台主机（假身份）：源在 source 连接上，目标在 target 连接上。
        let request = PasteRequest {
            op: 2,
            force_relay: false,
            side: Side::Remote,
            items: vec![ClipItem {
                path: format!("{}/tree", source.dir),
                is_dir: true,
            }],
            target_dir: format!("{}/in", target.dir),
            source: ClipSource {
                view: 99,
                server_key: Some("someone@other-host:22".to_owned()),
                provider: Some(Arc::downgrade(&src_provider)),
            },
            this_view: 1,
        };
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("跨主机粘贴");
        assert_eq!(outcome.method, Method::Relay);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].direction, Direction::RemoteCopy);
        let states = finish(&mut events, &jobs).await;
        assert!(
            states.iter().all(|(_, s)| matches!(s, TransferState::Done)),
            "{states:?}"
        );
        assert_eq!(
            get(
                &target.channel,
                &format!("{}/in/tree/sub/b.txt", target.dir)
            )
            .await,
            b"beta"
        );
        assert_eq!(
            get(
                &target.channel,
                &format!("{}/in/tree/中 文 c.txt", target.dir)
            )
            .await,
            "gamma".as_bytes()
        );

        // 源所在的连接已经关了（通道提供者没人持有）：说清楚怎么办，不悬挂。
        let dead = {
            let temp = Arc::new(SftpChannelProvider::new(
                Arc::clone(&source.connection),
                None,
            ));
            Arc::downgrade(&temp)
        };
        let request = PasteRequest {
            op: 3,
            force_relay: false,
            side: Side::Remote,
            items: vec![ClipItem {
                path: format!("{}/tree", source.dir),
                is_dir: true,
            }],
            target_dir: format!("{}/in", target.dir),
            source: ClipSource {
                view: 98,
                server_key: Some("someone@other-host:22".to_owned()),
                provider: Some(dead),
            },
            this_view: 1,
        };
        let error = run_paste(ctx.clone(), request)
            .await
            .err()
            .unwrap_or_default();
        assert!(error.contains("复制的源所在的连接已经关闭"), "{error}");
    });

    // 同一台主机 + 用户选了"改为经本机中转"：不用服务器端复制，走队列。
    let (ctx, mut events, _provider) = remote_ctx(&source, None);
    rt.block_on(async {
        source
            .channel
            .create_dir(&format!("{}/relay", source.dir))
            .await
            .expect("建目标");
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{}/tree", source.dir),
                is_dir: true,
            }],
            &format!("{}/relay", source.dir),
            true,
        );
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("强制中转");
        assert_eq!(outcome.method, Method::Relay);
        assert_eq!(jobs.len(), 1);
        let states = finish(&mut events, &jobs).await;
        assert!(
            states.iter().all(|(_, s)| matches!(s, TransferState::Done)),
            "{states:?}"
        );
        assert_eq!(
            get(&source.channel, &format!("{}/relay/tree/a.txt", source.dir)).await,
            b"alpha"
        );
    });
}

/// 合并进已有文件夹：顶层先问一次（「覆盖」= 合并这个文件夹），里面同名的文件再逐个问——不是整体覆盖，
/// 也不是悄悄合并。这里扮演界面：顶层回答「覆盖」，a.txt 回答「跳过」或「覆盖」。
#[test]
fn live_merging_into_an_existing_folder_asks_per_file() {
    let Some(live) = crate::live_support::LiveSftp::connect("g08b-merge") else {
        return;
    };
    let rt = runtime();
    for (file_answer, expected) in [
        (ConflictAction::Skip, b"old-a".as_slice()),
        (ConflictAction::Overwrite, b"alpha".as_slice()),
    ] {
        let (ctx, mut events, _provider, prompts) = remote_ctx_with_prompts(&live, None);
        let root = format!(
            "{}/{}",
            live.dir,
            if expected == b"alpha" { "ow" } else { "sk" }
        );
        let resolver = Arc::clone(&ctx.resolver);
        // 界面：顶层文件夹的同名问「覆盖」，文件级的 a.txt 问 `file_answer`，别的文件照常覆盖。
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let asked_in_thread = Arc::clone(&asked);
        let responder = std::thread::spawn(move || {
            while let Ok(event) = prompts.recv_timeout(Duration::from_secs(30)) {
                if let SftpEvent::Conflict { id, info } = event {
                    asked_in_thread
                        .lock()
                        .expect("lock")
                        .push(info.target.clone());
                    let action = if info.target.ends_with("/dest/tree") {
                        ConflictAction::Overwrite
                    } else if info.target.ends_with("/a.txt") {
                        file_answer
                    } else {
                        ConflictAction::Overwrite
                    };
                    resolver.respond(
                        id,
                        zssh_core::sftp::transfer::ConflictDecision {
                            action,
                            apply_to_all: false,
                            rename_to: None,
                        },
                    );
                }
            }
        });
        rt.block_on(async {
            live.channel.create_dir(&root).await.expect("建根");
            make_tree(&live.channel, &root).await;
            live.channel
                .create_dir(&format!("{root}/dest"))
                .await
                .expect("建目标");
            live.channel
                .create_dir(&format!("{root}/dest/tree"))
                .await
                .expect("建同名文件夹");
            put(&live.channel, &format!("{root}/dest/tree/a.txt"), b"old-a").await;
            let request = remote_request(
                &ctx,
                vec![ClipItem {
                    path: format!("{root}/tree"),
                    is_dir: true,
                }],
                &format!("{root}/dest"),
                false,
            );
            let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("合并粘贴");
            // 合并时有 copy-data 就在服务器上逐个问；没有就经本机中转让引擎逐个问。
            if !jobs.is_empty() {
                finish(&mut events, &jobs).await;
            }
            assert!(
                outcome
                    .items
                    .iter()
                    .all(|i| matches!(i.state, ItemState::Ok | ItemState::Queued)),
                "{:?}",
                outcome.items
            );
            assert_eq!(
                get(&live.channel, &format!("{root}/dest/tree/a.txt")).await,
                expected
            );
            // 没有同名的文件照常复制过来（也问了，回答覆盖）。
            assert_eq!(
                get(&live.channel, &format!("{root}/dest/tree/sub/b.txt")).await,
                b"beta"
            );
        });
        ctx.resolver.shutdown();
        drop(responder);
        let asked = asked.lock().expect("lock").clone();
        assert!(
            asked.iter().any(|t| t.ends_with("/dest/tree"))
                && asked.iter().any(|t| t.ends_with("/a.txt")),
            "顶层和 a.txt 都该被问到：{asked:?}"
        );
    }
}

/// 取消：开始前就取消了——所有条目都标「已取消」，目标里什么也没有写。
#[test]
fn live_cancelled_paste_writes_nothing() {
    let Some(live) = crate::live_support::LiveSftp::connect("g08b-cancel") else {
        return;
    };
    let (ctx, _events, _provider) = remote_ctx(&live, None);
    let root = live.dir.clone();
    let rt = runtime();
    rt.block_on(async {
        make_tree(&live.channel, &root).await;
        live.channel
            .create_dir(&format!("{root}/dest"))
            .await
            .expect("建目标");
        ctx.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{root}/tree"),
                is_dir: true,
            }],
            &format!("{root}/dest"),
            false,
        );
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("取消的粘贴");
        assert!(jobs.is_empty());
        assert_eq!(outcome.items[0].state, ItemState::Cancelled);
        let names = live
            .channel
            .read_dir(&format!("{root}/dest"))
            .await
            .expect("列目录");
        assert!(names.is_empty(), "取消后目标里不该有东西：{names:?}");
        let _ = ConflictAction::Skip;
    });
}

/// 服务器没有 copy-data（这里用开关模拟）：POSIX shell 就用 `cp -a`；`cp` 跑了但失败不会自动换办法，
/// 结果里标成"可以改经本机中转"。失败用一个 250 字节的名字触发：「 - 副本」一加就超过 255 字节，
/// 共用机上 root 不会因为权限失败。
#[test]
fn live_cp_copy_runs_on_the_server_and_a_failed_cp_offers_the_relay() {
    let Some(live) = crate::live_support::LiveSftp::connect("g08b-cp") else {
        return;
    };
    let (mut ctx, _events, _provider) = remote_ctx(&live, Some(ConflictAutomation::Rename));
    ctx.no_copy_data = true;
    let root = live.dir.clone();
    let rt = runtime();
    rt.block_on(async {
        make_tree(&live.channel, &root).await;
        live.channel
            .create_dir(&format!("{root}/dest"))
            .await
            .expect("建目标");
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{root}/tree"),
                is_dir: true,
            }],
            &format!("{root}/dest"),
            false,
        );
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("cp 粘贴");
        assert!(jobs.is_empty(), "{jobs:?}");
        assert_eq!(outcome.method, Method::ServerCp, "{:?}", outcome.items);
        assert_eq!(outcome.items[0].state, ItemState::Ok, "{:?}", outcome.items);
        assert_eq!(
            get(&live.channel, &format!("{root}/dest/tree/sub/b.txt")).await,
            b"beta"
        );

        // cp 失败：不自动换办法，结果里给"可以中转"。
        let long = "a".repeat(250);
        put(&live.channel, &format!("{root}/{long}"), b"long").await;
        let request = remote_request(
            &ctx,
            vec![ClipItem {
                path: format!("{root}/{long}"),
                is_dir: false,
            }],
            &root,
            false,
        );
        let (outcome, jobs) = run_paste(ctx.clone(), request).await.expect("长名字粘贴");
        assert!(jobs.is_empty(), "cp 失败不能悄悄换成中转：{jobs:?}");
        assert_eq!(outcome.method, Method::ServerCp);
        assert_eq!(
            outcome.items[0].state,
            ItemState::Failed,
            "{:?}",
            outcome.items
        );
        assert!(outcome.items[0].can_relay, "{:?}", outcome.items);
        let retry = clip::relay_retry_items(&outcome).expect("有可重试的条目");
        assert_eq!(retry.len(), 1);
    });
}
