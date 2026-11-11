//! 日志：写 stderr，或写入指定文件并按大小轮转（path → path.1 → path.2 … 最多保留 KEEP 份）。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::timefmt::log_stamp;

struct FileSink {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
}

static SINK: Mutex<Option<FileSink>> = Mutex::new(None);

/// 设置日志文件；不调用则写 stderr。
pub fn init_file(path: PathBuf, max_bytes: u64, keep: u32) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut g) = SINK.lock() {
        *g = Some(FileSink { path, max_bytes, keep });
    }
}

fn numbered(p: &Path, i: u32) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(format!(".{i}"));
    PathBuf::from(s)
}

/// 超限时轮转：删除 path.KEEP，path.(i) → path.(i+1)，path → path.1
fn rotate(s: &FileSink) {
    if s.keep == 0 {
        let _ = fs::remove_file(&s.path);
        return;
    }
    let _ = fs::remove_file(numbered(&s.path, s.keep));
    for i in (1..s.keep).rev() {
        let from = numbered(&s.path, i);
        if from.exists() {
            let _ = fs::rename(&from, numbered(&s.path, i + 1));
        }
    }
    let _ = fs::rename(&s.path, numbered(&s.path, 1));
}

/// 写一行日志：`[YYYY-MM-DD HH:MM:SS] LEVEL 内容`。写文件失败时退回 stderr，绝不 panic。
pub fn write(level: &str, msg: &str) {
    let line = format!("[{}] {:<5} {}\n", log_stamp(), level, msg);
    // 单线程使用；try_lock 避免 panic hook 在持锁时重入导致死锁
    let guard = SINK.try_lock();
    if let Ok(g) = guard {
        if let Some(s) = g.as_ref() {
            let size = fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0);
            if size > 0 && size + line.len() as u64 > s.max_bytes {
                rotate(s);
            }
            let open = || OpenOptions::new().create(true).append(true).open(&s.path);
            // 目录可能被删：先建目录再试一次
            let f = open().or_else(|_| {
                if let Some(dir) = s.path.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                open()
            });
            if let Ok(mut f) = f {
                // 轮转失败（如 rename 出错）导致主文件仍远超上限时，清空兜底
                if f.metadata().map(|m| m.len()).unwrap_or(0) > s.max_bytes.saturating_mul(2) {
                    let _ = f.set_len(0);
                }
                if f.write_all(line.as_bytes()).is_ok() {
                    return;
                }
            }
        }
    }
    let _ = std::io::stderr().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::logging::write("INFO", &format!($($t)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::logging::write("WARN", &format!($($t)*)) };
}
#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::logging::write("ERROR", &format!($($t)*)) };
}
