// 主线程栈放大到 8 MB（Windows 默认 1 MB）。
//
// GPUI 在主线程上递归布局 / 绘制元素树；主窗口把 AI 面板、文件面板嵌进同一棵树之后，调试构建里
// 「主窗口各层 + 一个很长的渲染函数（文件视图有几百个局部变量）」的栈帧加起来会超过 1 MB。
// 发行构建的栈帧小得多，这里放大对两种构建都无害。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
