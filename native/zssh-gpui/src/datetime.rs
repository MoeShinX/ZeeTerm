//! 与界面无关的时间小工具：本机时区偏移和「天数 → 公历年月日」。
//! SFTP 列表的修改时间、连接历史、进程列表的显示都用它们，不属于任何一个功能模块。

/// 本机时区相对 UTC 的秒数（列表里的时间按本地时间写）。
pub(crate) fn local_utc_offset() -> i64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut info = TIME_ZONE_INFORMATION::default();
        // SAFETY: `info` 在调用期间有效；返回值只是夏令时状态。
        let state = unsafe { GetTimeZoneInformation(&mut info) };
        // Bias 是“UTC = 本地 + Bias”（分钟），所以本地 = UTC - Bias；夏令时再减 DaylightBias。
        let extra = if state == 2 {
            info.DaylightBias
        } else {
            info.StandardBias
        };
        -(i64::from(info.Bias) + i64::from(extra)) * 60
    }
    #[cfg(unix)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as libc::time_t);
        let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: localtime_r 写入调用方拥有的有效存储，结果包含当前夏令时偏移。
        unsafe {
            if libc::localtime_r(&now, local.as_mut_ptr()).is_null() {
                return 0;
            }
            local.assume_init().tm_gmtoff
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        0
    }
}

/// 天数（自 1970-01-01）→ 公历年月日。
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_convert_to_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
