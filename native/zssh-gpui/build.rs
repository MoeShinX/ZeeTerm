// 主线程栈放大到 8 MB（Windows 默认 1 MB）。
//
// GPUI 在主线程上递归布局 / 绘制元素树；主窗口把 AI 面板、文件面板嵌进同一棵树之后，调试构建里
// 「主窗口各层 + 一个很长的渲染函数（文件视图有几百个局部变量）」的栈帧加起来会超过 1 MB。
// 发行构建的栈帧小得多，这里放大对两种构建都无害。
fn main() {
    embed_icon();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// 把 `assets/brand/zeeterm.ico` 编进 exe 的资源（ID 1，GPUI 的窗口类图标就按这个 ID 找），
/// 任务栏、Alt+Tab、窗口左上角和资源管理器里的 exe 图标因此都是 ZeeTerm 的 Logo。
/// 找不到 rc.exe 就让构建失败：悄悄少一个图标比构建报错更难发现。
#[cfg(windows)]
fn embed_icon() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        embed_resource::compile("assets/zeeterm.rc", embed_resource::NONE)
            .manifest_required()
            .expect("embedding assets/zeeterm.rc failed");
    }
    println!("cargo:rerun-if-changed=assets/zeeterm.rc");
    println!("cargo:rerun-if-changed=assets/brand/zeeterm.ico");
}

#[cfg(not(windows))]
fn embed_icon() {}
