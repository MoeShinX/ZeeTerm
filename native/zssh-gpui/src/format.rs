//! 字节量的统一显示格式。整个 crate 只此一份实现，1024 进制、一位小数：
//! - 体积（确认框、传输列表、备份摘要）带空格：`512 B` / `1.5 KB` / `3.0 GB`；
//! - 累计流量（状态栏 / 概览）不带空格：`512B` / `1.5KB` / `5.0GB`（同 0.2.14 `formatTraffic`）；
//! - 网速不带空格：`512B/s` / `1.5KB/s`（同 0.2.14 `formatNetSpeed`）。
//!
//! 不在这里的：`mib_gb_label`（固定 GB、位数可调）和 opencode 下载确认框的大小
//! （KB 取整、0 字节显示「未知」）语义不同，留在各自模块。

/// 文件 / 目录体积：`512 B` / `1.5 KB` / `3.0 GB` / `1.0 TB`。
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// 累计流量（字节 → 123B / 4.5KB / 6.7MB / 8.9GB / 1.2TB / 3.4PB）。
pub fn traffic(bytes: u64) -> String {
    if bytes == 0 {
        return "0B".to_owned();
    }
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// 网速自动换算：B/s → KB/s → MB/s → GB/s；非正数（含 NaN）一律 `0B/s`。
pub fn speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "0B/s".to_owned();
    }
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut value = bytes_per_sec;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_uses_space_and_one_decimal() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KB");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(1024 * 1024), "1.0 MB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(size(1024 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn traffic_keeps_zero_bytes_dense() {
        assert_eq!(traffic(0), "0B");
        assert_eq!(traffic(512), "512B");
        assert_eq!(traffic(1536), "1.5KB");
        assert_eq!(traffic(5 * 1024 * 1024 * 1024), "5.0GB");
    }

    #[test]
    fn speed_reports_zero_for_non_positive() {
        assert_eq!(speed(0.0), "0B/s");
        assert_eq!(speed(-5.0), "0B/s");
        assert_eq!(speed(f64::NAN), "0B/s");
        assert_eq!(speed(512.4), "512B/s");
        assert_eq!(speed(1536.0), "1.5KB/s");
        assert_eq!(speed(5.0 * 1024.0 * 1024.0), "5.0MB/s");
        // 超过最大单位就停在 GB/s，数值继续放大。
        assert_eq!(speed(3.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0), "3072.0GB/s");
    }
}
