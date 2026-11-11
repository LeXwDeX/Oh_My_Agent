//! 时间工具：复刻 Python `datetime.fromisoformat(...).timestamp()` 的取值规则，以及日志用的本地时间格式。

use serde_json::Value;

use crate::scan::truthy;

/// 复刻 server.py 的 iso_ms()：
/// `int(datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() * 1000)`，解析失败返回 None。
/// 浮点运算顺序与 CPython 一致（带时区：总微秒 / 1e6；无时区：本地秒 + 微秒 / 1e6），保证毫秒截断结果相同。
pub fn iso_ms(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if !truthy(v) {
        return None;
    }
    // 非字符串在 Python 里会因 .replace 抛异常 → None
    let s = v.as_str()?.replace('Z', "+00:00");
    let p = parse_iso(&s)?;
    let secs_f = match p.offset_us {
        Some(off) => {
            let days = days_from_civil(p.year, p.month, p.day);
            let local_us = (days * 86_400 + p.hour * 3600 + p.minute * 60 + p.second) * 1_000_000 + p.micro;
            (local_us - off) as f64 / 1e6
        }
        None => {
            let secs = local_mktime(&p)?;
            secs as f64 + p.micro as f64 / 1e6
        }
    };
    let ms = secs_f * 1000.0;
    if !ms.is_finite() {
        return None;
    }
    Some(ms as i64) // `as` 向零截断，与 Python int() 相同
}

struct Parsed {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    micro: i64,
    offset_us: Option<i64>,
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 解析 "HH[:MM[:SS[.ffffff]]]" 或 "HH[MM[SS[.ffffff]]]"，返回 (时, 分, 秒, 微秒)。
fn parse_hms(t: &str) -> Option<(i64, i64, i64, i64)> {
    let (core, frac) = match t.find(['.', ',']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let parts: Vec<&str> = if core.contains(':') {
        let v: Vec<&str> = core.split(':').collect();
        if v.iter().any(|p| p.len() != 2) {
            return None;
        }
        v
    } else {
        match core.len() {
            2 => vec![&core[0..2]],
            4 => vec![&core[0..2], &core[2..4]],
            6 => vec![&core[0..2], &core[2..4], &core[4..6]],
            _ => return None,
        }
    };
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let h = digits(parts[0])?;
    let m = match parts.get(1) {
        Some(p) => digits(p)?,
        None => 0,
    };
    let s = match parts.get(2) {
        Some(p) => digits(p)?,
        None => 0,
    };
    let us = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // 超过 6 位截断，不足 6 位右补零
            let mut buf: String = f.chars().take(6).collect();
            while buf.len() < 6 {
                buf.push('0');
            }
            digits(&buf)?
        }
    };
    Some((h, m, s, us))
}

fn parse_iso(s: &str) -> Option<Parsed> {
    if !s.is_ascii() {
        // Python 允许任意单字符分隔符；实际时间戳都是 ASCII，非 ASCII 一律视为解析失败
        return None;
    }
    let b = s.as_bytes();
    let (year, month, day, mut pos) = if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        (digits(&s[0..4])?, digits(&s[5..7])?, digits(&s[8..10])?, 10)
    } else if b.len() >= 8 && b[..8].iter().all(|c| c.is_ascii_digit()) {
        (digits(&s[0..4])?, digits(&s[4..6])?, digits(&s[6..8])?, 8)
    } else {
        return None;
    };
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut p = Parsed { year, month, day, hour: 0, minute: 0, second: 0, micro: 0, offset_us: None };
    if pos == b.len() {
        return Some(p);
    }
    pos += 1; // 日期与时间之间的任意单字符分隔符
    let rest = s.get(pos..)?;
    if rest.is_empty() {
        return None;
    }
    let (tpart, tz) = match rest.find(['+', '-']) {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let (h, mi, se, us) = parse_hms(tpart)?;
    if h == 24 {
        // 24:00:00 表示次日零点
        if mi != 0 || se != 0 || us != 0 {
            return None;
        }
    } else if !(0..=23).contains(&h) {
        return None;
    }
    if !(0..=59).contains(&mi) || !(0..=59).contains(&se) {
        return None;
    }
    p.hour = h;
    p.minute = mi;
    p.second = se;
    p.micro = us;
    if let Some(tz) = tz {
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let (oh, om, os, ous) = parse_hms(&tz[1..])?;
        if !(0..=59).contains(&om) || !(0..=59).contains(&os) {
            return None;
        }
        let off = ((oh * 3600 + om * 60 + os) * 1_000_000 + ous) * sign;
        if off.abs() >= 86_400 * 1_000_000 {
            return None;
        }
        p.offset_us = Some(off);
    }
    Some(p)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 算法）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 无时区时间按本地时区换算成 epoch 秒（同 Python 对 naive datetime 的处理）。
fn local_mktime(p: &Parsed) -> Option<i64> {
    // SAFETY: tm 为纯数据结构，零初始化合法；mktime 只读写传入的 tm。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (p.year - 1900) as libc::c_int;
    tm.tm_mon = (p.month - 1) as libc::c_int;
    tm.tm_mday = p.day as libc::c_int;
    tm.tm_hour = p.hour as libc::c_int; // 24 点由 mktime 自动进位到次日
    tm.tm_min = p.minute as libc::c_int;
    tm.tm_sec = p.second as libc::c_int;
    tm.tm_isdst = -1;
    let t = unsafe { libc::mktime(&mut tm) };
    if t == -1 {
        return None;
    }
    Some(t as i64)
}

/// 日志时间戳：本地时间 "YYYY-MM-DD HH:MM:SS"。
pub fn log_stamp() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&now, &mut tm).is_null() };
    if !ok {
        return String::from("????-??-?? ??:??:??");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn utc_ms() {
        assert_eq!(iso_ms(Some(&json!("2026-01-01T00:00:00.123Z"))), Some(1767225600123));
        assert_eq!(iso_ms(Some(&json!("2026-01-01T08:00:00+08:00"))), Some(1767225600000));
        assert_eq!(iso_ms(Some(&json!("bad"))), None);
        assert_eq!(iso_ms(Some(&json!(""))), None);
        assert_eq!(iso_ms(Some(&json!(123))), None);
    }
}
