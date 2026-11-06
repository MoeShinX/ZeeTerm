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
        // 此刻生效的偏移（含夏令时）；libc 的细节放在 core 里，这里不重复写 unsafe。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or_default();
        zssh_core::ssh::session_log::local_utc_offset_secs(now)
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
    fn the_local_offset_is_a_real_time_zone_offset() {
        // 地球上的时区在 UTC-12 到 UTC+14 之间；具体值取决于运行机器的时区，所以只看范围。
        let offset = local_utc_offset();
        assert!((-12 * 3600..=14 * 3600).contains(&offset), "{offset}");
    }

    #[test]
    fn days_convert_to_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
