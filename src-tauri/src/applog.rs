//! Always-on session logging — one file per app session under
//! `<app data>\logs\session-<epoch>.log`, compacted to the newest ~4000
//! lines and pruned to the newest 20 sessions. Every network, transfer,
//! membership and tray boundary writes here so a bug report comes with
//! the actual trace instead of code archaeology.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<AppLog>> = OnceLock::new();

struct AppLog {
    file: std::fs::File,
    path: PathBuf,
    lines: usize,
}

/// Compact when a file passes this many lines…
const MAX_LINES: usize = 5000;
/// …down to the newest this many.
const KEEP_LINES: usize = 4000;
/// Retain at most this many session files total.
const MAX_FILES: usize = 20;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn timestamp() -> String {
    let t = SystemTime::now();
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let millis = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    // civil-from-days — no chrono dep.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil(days as i64);
    format!(
        "{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{millis:03}"
    )
}

fn civil(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Create the session log under `dir/logs/` and prune old sessions.
/// Call once at startup; safe to call again (keeps the first session).
pub fn init(dir: &Path) {
    if LOG.get().is_some() {
        return;
    }
    let logs = dir.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(&logs)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| {
                            let n = n.to_string_lossy();
                            n.starts_with("session-") && n.ends_with(".log")
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    sessions.sort(); // names sort by epoch — oldest first
    while sessions.len() >= MAX_FILES {
        let _ = std::fs::remove_file(sessions.remove(0));
    }
    let path = logs.join(format!("session-{}.log", now_secs()));
    match std::fs::File::create(&path) {
        Ok(f) => {
            let _ = LOG.set(Mutex::new(AppLog {
                file: f,
                path,
                lines: 0,
            }));
            line("app", "session log started");
        }
        Err(e) => log::warn!("applog: can't create {}: {e}", path.display()),
    }
}

/// Append one line — `[timestamp] [tag] msg`. Compacts the file when it
/// passes MAX_LINES so it always holds the newest KEEP_LINES.
pub fn line(tag: &str, msg: &str) {
    log::info!("[{tag}] {msg}"); // dev console parity via tauri-plugin-log
    let Some(mu) = LOG.get() else { return };
    let Ok(mut l) = mu.lock() else { return };
    for m in msg.lines() {
        let _ = writeln!(l.file, "[{}] [{tag}] {m}", timestamp());
        l.lines += 1;
    }
    let _ = l.file.flush();
    if l.lines > MAX_LINES {
        compact(&mut l);
    }
}

/// Rewrite the file keeping only the newest KEEP_LINES lines.
fn compact(l: &mut AppLog) {
    let Ok(body) = std::fs::read_to_string(&l.path) else { return };
    let kept: Vec<&str> = body.lines().collect();
    let start = kept.len().saturating_sub(KEEP_LINES);
    let mut out = String::with_capacity(body.len().min(128 * 1024));
    out.push_str(&format!(
        "[{}] [app] — compacted: dropped {} older line(s)\n",
        timestamp(),
        start
    ));
    for k in &kept[start..] {
        out.push_str(k);
        out.push('\n');
    }
    if let Ok(mut f) = std::fs::File::create(&l.path) {
        if f.write_all(out.as_bytes()).is_ok() {
            l.file = f;
            l.lines = kept.len() - start + 1;
        }
    }
}

/// The current session's log path (None if logging never initialized).
pub fn path() -> Option<PathBuf> {
    LOG.get().and_then(|m| m.lock().ok().map(|l| l.path.clone()))
}

/// The newest `n` lines of the current session log.
pub fn tail(n: usize) -> Vec<String> {
    let Some(p) = path() else { return vec![] };
    let Ok(body) = std::fs::read_to_string(&p) else { return vec![] };
    let lines: Vec<String> = body.lines().map(|l| l.to_string()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// One-line helper so call sites stay terse: `alog!("xfer", "msg")`.
#[macro_export]
macro_rules! alog {
    ($tag:expr, $($arg:tt)*) => {
        $crate::applog::line($tag, &format!($($arg)*))
    };
}
