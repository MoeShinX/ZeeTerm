//! 读文件和展开 `Include`：把配置整理成按出现顺序排好的一串「段」（`Host` / `Match` / 文件开头）。
//!
//! 读文件只通过 [`ConfigFs`]，生产里是只读的 [`RealFs`]，测试里是内存里的假文件系统。

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use super::syntax::{glob_match, has_wildcard, parse_line};
use super::{ConfigNotice, ParseOptions};

/// `Include` 最多嵌套几层（和 OpenSSH 一致）。
pub(super) const MAX_INCLUDE_DEPTH: usize = 16;
/// 单个文件最大读多少字节：真实的配置文件远小于这个数，超过的多半不是配置文件。
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// 读配置文件用的文件系统。只有读，没有写——导入不会改动任何文件。
pub trait ConfigFs {
    /// 读文件内容（最多 [`MAX_FILE_BYTES`] + 1 个字节，超出就是太大了）。
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// 目录里的条目名（只有名字，没有路径）。
    fn list_dir(&self, dir: &Path) -> std::io::Result<Vec<String>>;
    /// 是不是一个普通文件（目录、不存在都是否）。
    fn is_file(&self, path: &Path) -> bool;
}

/// 真实的文件系统，只读。
pub struct RealFs;

impl ConfigFs for RealFs {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn list_dir(&self, dir: &Path) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn is_file(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
    }
}

/// 主配置文件没读到。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    NotFound,
    TooLarge,
    /// 其他读不了的原因（权限、被占用……），带系统给的说明。
    Unreadable(String),
}

/// 一段设置生效的范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Scope {
    /// 文件开头、第一个 `Host` / `Match` 之前：对所有主机生效（等同 `Host *`）。
    Global,
    /// `Host` 行的模式列表。
    Hosts(Vec<String>),
    /// `Match` 段：整段不导入。
    Match,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Setting {
    pub key: String,
    pub key_raw: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Block {
    pub scope: Scope,
    pub settings: Vec<Setting>,
}

pub(super) struct Loaded {
    pub blocks: Vec<Block>,
    pub notices: Vec<ConfigNotice>,
}

pub(super) fn load(
    fs: &dyn ConfigFs,
    root: &Path,
    options: &ParseOptions,
) -> Result<Loaded, ReadError> {
    let bytes = fs.read(root).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound,
        _ => ReadError::Unreadable(err.to_string()),
    })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ReadError::TooLarge);
    }
    let mut loader = Loader {
        fs,
        options,
        blocks: Vec::new(),
        notices: Vec::new(),
        stack: vec![path_key(root)],
    };
    loader.process(root, &decode(&bytes), 0, Scope::Global);
    Ok(Loaded {
        blocks: loader.blocks,
        notices: loader.notices,
    })
}

/// 去掉 UTF-8 BOM（Windows 的编辑器常带），不是合法 UTF-8 的字节用替换符顶上。
fn decode(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

struct Loader<'a> {
    fs: &'a dyn ConfigFs,
    options: &'a ParseOptions,
    blocks: Vec<Block>,
    notices: Vec<ConfigNotice>,
    /// 正在展开的文件（从根到当前），用来发现 `Include` 绕回来。
    stack: Vec<String>,
}

impl Loader<'_> {
    /// 处理一个文件。`entry` 是它被读进来时已经处于的范围：主文件是 `Global`，
    /// 被 `Include` 的文件是 `Include` 那一行所在的范围（所以写在 `Host` 段里的 `Include`，
    /// 它的内容就只对那个 `Host` 生效）。文件里的 `Host` / `Match` 只在这个文件内部切换范围，
    /// 读完回到 `Include` 之前的范围。
    fn process(&mut self, file: &Path, text: &str, depth: usize, entry: Scope) {
        let mut current = entry;
        self.start_block(current.clone());
        for (index, line) in text.lines().enumerate() {
            let Some(directive) = parse_line(line) else {
                continue;
            };
            match directive.key.as_str() {
                "host" => {
                    current = Scope::Hosts(directive.args);
                    self.start_block(current.clone());
                }
                "match" => {
                    self.notices.push(ConfigNotice::MatchSkipped {
                        file: self.shown(file),
                        line: index + 1,
                    });
                    current = Scope::Match;
                    self.start_block(current.clone());
                }
                "include" => {
                    // Match 段里的 Include 跟着整段一起不处理。
                    if current != Scope::Match {
                        for arg in &directive.args {
                            self.include(arg, depth, &current);
                        }
                        // 被包含的文件自己会开新段；回来后接着当前范围继续。
                        self.start_block(current.clone());
                    }
                }
                _ => {
                    if let Some(block) = self.blocks.last_mut() {
                        block.settings.push(Setting {
                            key: directive.key,
                            key_raw: directive.key_raw,
                            args: directive.args,
                        });
                    }
                }
            }
        }
    }

    fn start_block(&mut self, scope: Scope) {
        self.blocks.push(Block {
            scope,
            settings: Vec::new(),
        });
    }

    /// 展开 `Include` 的一个参数：支持通配符；相对路径相对 `ssh_dir`；按文件名排序。
    fn include(&mut self, arg: &str, depth: usize, scope: &Scope) {
        let wildcard = has_wildcard(arg);
        let pattern = self.resolve(arg);
        let files = if wildcard {
            self.glob(&pattern)
        } else {
            vec![pattern]
        };
        for path in files {
            self.include_file(&path, depth + 1, scope);
        }
    }

    fn include_file(&mut self, path: &Path, depth: usize, scope: &Scope) {
        if depth > MAX_INCLUDE_DEPTH {
            self.notices.push(ConfigNotice::IncludeTooDeep {
                file: self.shown(path),
            });
            return;
        }
        let key = path_key(path);
        if self.stack.contains(&key) {
            self.notices.push(ConfigNotice::IncludeLoop {
                file: self.shown(path),
            });
            return;
        }
        match self.fs.read(path) {
            Ok(bytes) if bytes.len() > MAX_FILE_BYTES => {
                self.notices.push(ConfigNotice::FileTooLarge {
                    file: self.shown(path),
                });
            }
            Ok(bytes) => {
                self.stack.push(key);
                self.process(path, &decode(&bytes), depth, scope.clone());
                self.stack.pop();
            }
            // OpenSSH 对不存在的 Include 文件也是悄悄跳过。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => self.notices.push(ConfigNotice::IncludeUnreadable {
                file: self.shown(path),
                error: err.to_string(),
            }),
        }
    }

    /// `~` 换成用户目录；相对路径接在 `ssh_dir` 后面；绝对路径不动。
    fn resolve(&self, arg: &str) -> PathBuf {
        if let Some(rest) = arg
            .strip_prefix("~/")
            .or_else(|| arg.strip_prefix("~\\"))
            .or_else(|| (arg == "~").then_some(""))
        {
            return join_native(&self.options.home, rest);
        }
        let path = Path::new(arg);
        if path.is_absolute() || path.has_root() {
            path.to_path_buf()
        } else {
            join_native(&self.options.ssh_dir, arg)
        }
    }

    /// 按路径的每一段展开通配符，只留下普通文件，按路径排序。
    fn glob(&self, pattern: &Path) -> Vec<PathBuf> {
        let mut bases = vec![PathBuf::new()];
        for component in pattern.components() {
            match component {
                Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    if has_wildcard(&name) {
                        let mut next = Vec::new();
                        for base in &bases {
                            let dir = if base.as_os_str().is_empty() {
                                Path::new(".")
                            } else {
                                base.as_path()
                            };
                            let Ok(mut names) = self.fs.list_dir(dir) else {
                                continue;
                            };
                            names.sort();
                            for entry in names {
                                // 和 glob(3) 一样，通配符不匹配以点开头的名字，除非模式本身以点开头。
                                if entry.starts_with('.') && !name.starts_with('.') {
                                    continue;
                                }
                                if glob_match(&name, &entry) {
                                    next.push(base.join(entry));
                                }
                            }
                        }
                        bases = next;
                    } else {
                        for base in &mut bases {
                            base.push(name.as_ref());
                        }
                    }
                }
                other => {
                    for base in &mut bases {
                        base.push(other.as_os_str());
                    }
                }
            }
        }
        let mut files: Vec<PathBuf> = bases
            .into_iter()
            .filter(|path| self.fs.is_file(path))
            .collect();
        files.sort();
        files.dedup();
        files
    }

    fn shown(&self, path: &Path) -> String {
        display_file(path, &self.options.ssh_dir)
    }
}

/// 把 `rest`（用 `/` 或 `\` 分隔都行）一段一段接到 `base` 后面，得到当前平台的写法。
pub(super) fn join_native(base: &Path, rest: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for part in rest.split(['/', '\\']).filter(|part| !part.is_empty()) {
        path.push(part);
    }
    path
}

/// 提示里显示的文件名：在 `ssh_dir` 里面就写相对路径（用 `/` 分隔），在外面写完整路径。
pub(super) fn display_file(path: &Path, ssh_dir: &Path) -> String {
    match path.strip_prefix(ssh_dir) {
        Ok(rest) if !rest.as_os_str().is_empty() => rest
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        _ => path.display().to_string(),
    }
}

/// 比较两个路径是不是同一个文件用的键：只做词法整理（`.`、`..`），不碰文件系统；Windows 上不分大小写。
pub(super) fn path_key(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_string_lossy().replace('\\', "/")),
        }
    }
    let key = parts.join("/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}
