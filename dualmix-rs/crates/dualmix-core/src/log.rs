//! Tiny thread-safe file logger with size based rotation (same format as the C# `Log`), app data paths and
//! local-time helpers (no external crates).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_SIZE: u64 = 1024 * 1024;

static GATE: Mutex<()> = Mutex::new(());
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static ECHO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `%APPDATA%\DualMix` (created on first use).
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let dir = base.join("DualMix");
            let _ = fs::create_dir_all(&dir);
            dir
        })
        .clone()
}

pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn log_file() -> PathBuf {
    data_dir().join("dualmix.log")
}

/// Also print every line on stderr (diagnostics / console runs).
pub fn set_echo(on: bool) {
    ECHO.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn info(msg: impl AsRef<str>) {
    write("INFO ", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write("WARN ", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write("ERROR", msg.as_ref());
}

fn write(level: &str, message: &str) {
    let line = format!("{} {} [{}] {}\r\n", timestamp(), level, thread_id(), message);
    if ECHO.load(std::sync::atomic::Ordering::Relaxed) {
        eprint!("{line}");
    }
    let _g = GATE.lock();
    let path = log_file();
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_SIZE {
            let old = path.with_extension("log.old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, &old);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(windows)]
fn thread_id() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    unsafe { GetCurrentThreadId() }
}

#[cfg(not(windows))]
fn thread_id() -> u32 {
    0
}

// ------------------------------------------------------------------------------------------ time

#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
    /// Offset from UTC in minutes (local = UTC + offset).
    pub offset_min: i32,
}

#[cfg(windows)]
pub fn local_now() -> LocalTime {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        y: u16,
        mo: u16,
        dow: u16,
        d: u16,
        h: u16,
        mi: u16,
        s: u16,
        ms: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
        fn GetSystemTime(t: *mut SystemTime);
    }
    let mut l = SystemTime::default();
    let mut u = SystemTime::default();
    unsafe {
        GetSystemTime(&mut u);
        GetLocalTime(&mut l);
    }
    let lm = days_from_civil(l.y as i64, l.mo as u32, l.d as u32) * 1440 + l.h as i64 * 60 + l.mi as i64;
    let um = days_from_civil(u.y as i64, u.mo as u32, u.d as u32) * 1440 + u.h as i64 * 60 + u.mi as i64;
    LocalTime {
        year: l.y as i32,
        month: l.mo as u32,
        day: l.d as u32,
        hour: l.h as u32,
        minute: l.mi as u32,
        second: l.s as u32,
        millis: l.ms as u32,
        offset_min: (lm - um) as i32,
    }
}

#[cfg(not(windows))]
pub fn local_now() -> LocalTime {
    let secs = unix_now();
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    LocalTime { year: y as i32, month: m, day: d, hour: (rem / 3600) as u32, minute: ((rem / 60) % 60) as u32, second: (rem % 60) as u32, millis: 0, offset_min: 0 }
}

pub fn timestamp() -> String {
    let t = local_now();
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", t.year, t.month, t.day, t.hour, t.minute, t.second, t.millis)
}

/// Seconds since the Unix epoch (UTC).
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local time as ISO 8601 with offset, like .NET `DateTime.Now` serialised by System.Text.Json.
pub fn now_iso() -> String {
    iso_from_unix(unix_now())
}

/// ISO 8601 local time (with the current local offset) for a Unix time.
pub fn iso_from_unix(unix: i64) -> String {
    let off = local_now().offset_min as i64;
    let local = unix + off * 60;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        y, m, d, rem / 3600, (rem / 60) % 60, rem % 60, sign, a / 60, a % 60
    )
}

/// Parses an ISO 8601 date-time ("2026-11-13T12:34:56.789+01:00", "...Z", or local without offset) to Unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.chars().take_while(|c| c.is_ascii_digit()).count();
        rest = &r[n..];
    }
    let offset_min = if rest.is_empty() {
        local_now().offset_min as i64
    } else if rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let hh = rest.get(1..3)?.parse::<i64>().ok()?;
        let mm = rest.get(4..6).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
        sign * (hh * 60 + mm)
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(days * 86400 + h * 3600 + mi * 60 + se - offset_min * 60)
}

/// Hours elapsed since an ISO time stored in the settings (None when missing / unreadable).
pub fn hours_since(iso: Option<&str>) -> Option<f64> {
    let t = parse_iso(iso?)?;
    Some((unix_now() - t) as f64 / 3600.0)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip() {
        let t = parse_iso("2026-11-13T12:00:00.1234567+01:00").unwrap();
        assert_eq!(t, parse_iso("2026-11-13T11:00:00Z").unwrap());
        let now = unix_now();
        let back = parse_iso(&iso_from_unix(now)).unwrap();
        assert_eq!(back, now);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }
}
