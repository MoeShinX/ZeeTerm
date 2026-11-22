//! 两段确认的通用状态机：第一次点击只「装填」，第二次（`take`）才真的执行。
//!
//! 各处界面（清除同步设置、退出 OneDrive、恢复备份、覆盖导出包）共用这一个类型；
//! 不需要携带载荷的地方用 `ArmedConfirm<()>`，要记住「确认的是哪一行 / 哪个路径」的
//! 用 `ArmedConfirm<T>` 把载荷存进来。

/// 装填过但还没取走的确认状态；载荷 `T` 是确认针对的目标（路径、行标识……）。
#[derive(Debug)]
pub(crate) struct ArmedConfirm<T> {
    armed: Option<T>,
}

// `Option<T>` 不管 `T` 能不能 Default 都是空的，所以这里手写而不是 derive（derive 会
// 反过来要求 `T: Default`）。
impl<T> Default for ArmedConfirm<T> {
    fn default() -> Self {
        Self { armed: None }
    }
}

impl<T> ArmedConfirm<T> {
    /// 装填；再次点击别的目标会替换掉原来的。
    pub(crate) fn arm(&mut self, value: T) {
        self.armed = Some(value);
    }

    /// 取消（点别处、刷新、关闭面板都该调它）。
    pub(crate) fn disarm(&mut self) {
        self.armed = None;
    }

    /// 现在确认状态针对的目标。
    pub(crate) fn armed(&self) -> Option<&T> {
        self.armed.as_ref()
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// 取走确认状态：只有装填过的那一次拿得到，防止连点执行两次。
    pub(crate) fn take(&mut self) -> Option<T> {
        self.armed.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_once_and_takes_once() {
        let mut confirm = ArmedConfirm::default();
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "第一次点击只进入确认状态");

        confirm.arm(());
        assert!(confirm.is_armed());
        assert!(confirm.take().is_some(), "第二次点击才真的执行");
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "确认状态只能用一次");

        confirm.arm(());
        confirm.disarm();
        assert!(confirm.take().is_none(), "取消后不能再确认");
    }

    #[test]
    fn remembers_what_it_is_armed_for() {
        let mut confirm = ArmedConfirm::default();
        confirm.arm("a");
        assert_eq!(confirm.armed(), Some(&"a"));
        confirm.arm("b");
        assert_eq!(confirm.armed(), Some(&"b"), "换目标就替换");
        assert_eq!(confirm.take(), Some("b"));
        assert_eq!(confirm.armed(), None);
    }
}
