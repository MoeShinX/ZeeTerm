//! 发版小工具：生成更新签名密钥、给安装包签名、生成 `latest.json`、验签。
//!
//! 不需要装 minisign.exe：签名格式（minisign、签名里带 `version:`）和应用里验签用的是同一套代码
//! （`zssh_update`）。由 `scripts/updater-keygen.ps1` 和 `scripts/release.ps1` 调用，也可以手动跑：
//!
//! ```text
//! cargo run --release --manifest-path native/Cargo.toml -p zssh-update --example release_tool -- <子命令> ...
//!   keygen   --out-dir <目录>                  生成 updater.key（私钥）和 updater.pub（公钥），已存在就拒绝
//!   pubkey   --key <私钥文件>                  打印 updater.json 里 pubkey 的格式（base64）
//!   sign     --key <私钥> --file <安装包> --version <x.y.z> --pubkey <base64> --out <签名文件>
//!   manifest --version <x.y.z> --notes-file <文件> --pub-date <RFC3339> --url <地址>
//!            --signature-file <签名文件> --out <latest.json>
//!   verify   --pubkey <base64> --file <安装包> --signature-file <签名文件> --version <x.y.z>
//! ```
//!
//! 私钥口令只从环境变量 `ZEETERM_UPDATER_KEY_PASSWORD` 读（keygen 时非空就加密私钥；sign / pubkey
//! 时私钥加密了就必须给），不从命令行参数读，免得出现在进程列表和历史记录里。输出里不打印私钥。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use minisign::{KeyPair, PublicKey, SecretKey, SecretKeyBox};
use zssh_update::{ReleaseManifest, build_latest_json, trusted_comment, verify_signature};

const PASSWORD_ENV: &str = "ZEETERM_UPDATER_KEY_PASSWORD";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .ok_or("缺少子命令（keygen / pubkey / sign / manifest / verify）")?;
    let options = parse_options(args)?;
    match command.as_str() {
        "keygen" => keygen(&options),
        "pubkey" => pubkey(&options),
        "sign" => sign(&options),
        "manifest" => manifest(&options),
        "verify" => verify(&options),
        other => Err(format!("不认识的子命令：{other}")),
    }
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<BTreeMap<String, String>, String> {
    let mut options = BTreeMap::new();
    let mut args = args;
    while let Some(flag) = args.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("参数格式不对：{flag}（应为 --名字 值）"))?;
        let value = args.next().ok_or_else(|| format!("参数 --{name} 缺少值"))?;
        options.insert(name.to_owned(), value);
    }
    Ok(options)
}

fn required<'a>(options: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, String> {
    options
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("缺少参数 --{name}"))
}

fn password() -> Option<String> {
    std::env::var(PASSWORD_ENV)
        .ok()
        .filter(|value| !value.is_empty())
}

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// 新建文件并写入；已存在就拒绝（`create_new`），绝不覆盖别人的私钥。
fn write_new(path: &Path, content: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            format!(
                "写 {} 失败（已存在的文件不会被覆盖）：{err}",
                path.display()
            )
        })?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|err| format!("写 {} 失败：{err}", path.display()))
}

fn keygen(options: &BTreeMap<String, String>) -> Result<(), String> {
    let dir = PathBuf::from(required(options, "out-dir")?);
    let key_path = dir.join("updater.key");
    let pub_path = dir.join("updater.pub");
    if key_path.exists() || pub_path.exists() {
        return Err(format!(
            "{} 里已经有密钥文件，拒绝覆盖（要重新生成请先自己确认旧密钥不再需要）",
            dir.display()
        ));
    }
    fs::create_dir_all(&dir).map_err(|err| format!("创建 {} 失败：{err}", dir.display()))?;

    let pair = match password() {
        Some(pw) => KeyPair::generate_encrypted_keypair(Some(pw)),
        None => KeyPair::generate_unencrypted_keypair(),
    }
    .map_err(|err| format!("生成密钥失败：{err}"))?;
    let secret = pair
        .sk
        .to_box(Some("ZeeTerm updater secret key"))
        .map_err(|err| format!("序列化私钥失败：{err}"))?
        .to_string();
    let public = pair
        .pk
        .to_box()
        .map_err(|err| format!("序列化公钥失败：{err}"))?
        .to_string();
    write_new(&key_path, &secret)?;
    write_new(&pub_path, &public)?;
    println!("私钥：{}", key_path.display());
    println!("公钥文件：{}", pub_path.display());
    println!("PUBKEY={}", b64(&public));
    Ok(())
}

/// 读私钥：没加密就直接用，加密了就用环境变量里的口令解开。
fn load_secret(path: &str) -> Result<SecretKey, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("读私钥 {path} 失败：{err}"))?;
    if let Ok(key) = SecretKey::from_unencrypted_box(
        SecretKeyBox::from_string(&text).map_err(|err| format!("私钥文件格式不对：{err}"))?,
    ) {
        return Ok(key);
    }
    let Some(pw) = password() else {
        return Err(format!(
            "私钥有口令保护：请先设置环境变量 {PASSWORD_ENV}（脚本会提示输入）"
        ));
    };
    SecretKey::from_box(
        SecretKeyBox::from_string(&text).map_err(|err| format!("私钥文件格式不对：{err}"))?,
        Some(pw),
    )
    .map_err(|err| format!("解开私钥失败（口令不对？）：{err}"))
}

fn pubkey_b64(pk: &PublicKey) -> Result<String, String> {
    pk.to_box()
        .map(|boxed| b64(&boxed.to_string()))
        .map_err(|err| format!("序列化公钥失败：{err}"))
}

fn pubkey(options: &BTreeMap<String, String>) -> Result<(), String> {
    let sk = load_secret(required(options, "key")?)?;
    let pk = PublicKey::from_secret_key(&sk).map_err(|err| format!("推公钥失败：{err}"))?;
    println!("{}", pubkey_b64(&pk)?);
    Ok(())
}

fn sign(options: &BTreeMap<String, String>) -> Result<(), String> {
    let version = required(options, "version")?;
    let file = required(options, "file")?;
    let out = required(options, "out")?;
    let sk = load_secret(required(options, "key")?)?;
    let pk = PublicKey::from_secret_key(&sk).map_err(|err| format!("推公钥失败：{err}"))?;

    let data = fs::read(file).map_err(|err| format!("读安装包 {file} 失败：{err}"))?;
    let name = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("安装包文件名不是有效文本")?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let comment = trusted_comment(version, name, timestamp).map_err(|err| err.to_string())?;
    let signed = minisign::sign(
        Some(&pk),
        &sk,
        data.as_slice(),
        Some(&comment),
        Some("ZeeTerm update signature"),
    )
    .map_err(|err| format!("签名失败：{err}"))?;
    let signature = b64(&signed.to_string());

    // 签完立刻用"配置里的公钥"按应用里的同一套代码验一遍：公钥和私钥对不上、版本绑定不对，
    // 在这里就失败，而不是等用户装的时候才发现。
    let expected_pubkey = match options.get("pubkey").filter(|value| !value.is_empty()) {
        Some(value) => value.clone(),
        None => pubkey_b64(&pk)?,
    };
    verify_signature(&data, &signature, &expected_pubkey, version, true).map_err(|err| {
        format!("签完自检没通过（私钥与 updater.json 里的公钥是一对吗？）：{err}")
    })?;

    fs::write(out, &signature).map_err(|err| format!("写签名文件 {out} 失败：{err}"))?;
    println!("已签名并自检通过：{out}");
    Ok(())
}

fn manifest(options: &BTreeMap<String, String>) -> Result<(), String> {
    let notes = fs::read_to_string(required(options, "notes-file")?)
        .map_err(|err| format!("读更新说明失败：{err}"))?;
    let signature = fs::read_to_string(required(options, "signature-file")?)
        .map_err(|err| format!("读签名文件失败：{err}"))?;
    let json = build_latest_json(&ReleaseManifest {
        version: required(options, "version")?,
        // 记事本等工具存的 UTF-8 文件带 BOM：不剥掉它会混进更新说明的第一个字。
        notes: notes.trim_start_matches('\u{feff}').trim(),
        pub_date: required(options, "pub-date")?,
        url: required(options, "url")?,
        signature: &signature,
    })
    .map_err(|err| err.to_string())?;
    let out = required(options, "out")?;
    fs::write(out, json).map_err(|err| format!("写 {out} 失败：{err}"))?;
    println!("已生成：{out}");
    Ok(())
}

fn verify(options: &BTreeMap<String, String>) -> Result<(), String> {
    let file = required(options, "file")?;
    let data = fs::read(file).map_err(|err| format!("读安装包 {file} 失败：{err}"))?;
    let signature = fs::read_to_string(required(options, "signature-file")?)
        .map_err(|err| format!("读签名文件失败：{err}"))?;
    verify_signature(
        &data,
        signature.trim(),
        required(options, "pubkey")?,
        required(options, "version")?,
        true,
    )
    .map_err(|err| err.to_string())?;
    println!("验签通过（含版本绑定）：{file}");
    Ok(())
}
