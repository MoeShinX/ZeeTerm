//! 生成 `models-dev-snapshot.json`：把完整的 models.dev `api.json` 裁成内置快照。
//!
//! 用法（一般通过 `scripts/models-dev-snapshot.ps1` 调用，它负责下载）：
//! `cargo run -p zssh-core --example models_dev_snapshot -- <api.json> <输出路径> <YYYY-MM-DD>`

use zssh_core::ai::agent::builtin::catalog::build_snapshot_json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, date] = args.as_slice() else {
        eprintln!("用法：models_dev_snapshot <api.json> <输出路径> <YYYY-MM-DD>");
        std::process::exit(2);
    };
    let api = std::fs::read(input).unwrap_or_else(|e| {
        eprintln!("读取 {input} 失败：{e}");
        std::process::exit(1);
    });
    let json = build_snapshot_json(&api, date).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    std::fs::write(output, json.as_bytes()).unwrap_or_else(|e| {
        eprintln!("写入 {output} 失败：{e}");
        std::process::exit(1);
    });
    println!("已写入 {output}（{} 字节）", json.len());
}
