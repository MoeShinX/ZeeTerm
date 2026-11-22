//! 打开本机主机库。主机窗口、AI 工具桥、转发运行时、更新器、智能体设置等
//! 与界面无关的使用方共用的入口，不放在任何一个窗口模块里。

use zeeterm_core::store::Database;

/// 打开主机库时的错误文案。
#[derive(Clone, Copy, Debug)]
enum HostDbMsg {
    CreateDataDirFailedPrefix,
}

impl crate::i18n::Msg for HostDbMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::CreateDataDirFailedPrefix => "创建数据目录失败：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::CreateDataDirFailedPrefix => "Could not create the data directory: ",
        }
    }
}

/// 打开（必要时先建）数据目录下的 `zeeterm.db`。
pub(crate) fn open_database() -> Result<Database, zeeterm_core::error::ZeetermError> {
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir).map_err(|err| {
        // 前缀已经按界面语言生成，没有稳定 key，原样显示。
        zeeterm_core::error::ZeetermError::InvalidInput(zeeterm_core::error::Reason::text(format!(
            "{}{err}",
            crate::i18n::t(HostDbMsg::CreateDataDirFailedPrefix)
        )))
    })?;
    Database::open(&zeeterm_core::store::db_path(&dir))
}
