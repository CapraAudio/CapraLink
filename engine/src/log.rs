//! The log file: `capralink.log` in the config dir, shared by the engine and the window
//! (appends from both processes are safe). Rotates to `capralink.1.log` at 1 MB.
//! Never log PINs, keys, tokens, pairing secrets or audio; device ids only as 8-char prefixes.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

const LIMIT: u64 = 1 << 20;
pub(crate) const FILE: &str = "capralink.log";
pub(crate) const OLD: &str = "capralink.1.log";
static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static ROLE: OnceLock<&'static str> = OnceLock::new();

/// Logs to the config dir (`None` = the OS one) as `role` ("daemon", "capralinkd", "ui"); the
/// first role set wins (the daemon sets its own before its node starts). Also logs panics.
pub fn init(config_dir: Option<PathBuf>, role: &'static str) {
    ROLE.get_or_init(|| role);
    let Ok(dir) = crate::node::config_dir_or_default(config_dir) else { return };
    *PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.join(FILE));
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log(&format!("panic: {info}"));
            default(info);
        }));
    });
}

/// Appends one timestamped line (dropped if logging isn't set up or the file can't be written).
pub fn log(msg: &str) {
    let path = PATH.lock().unwrap_or_else(|e| e.into_inner()); // held: one writer per process
    if let Some(p) = &*path {
        let _ = append(p, &format!("{} {} {msg}\n", now(), ROLE.get().unwrap_or(&"?")), LIMIT);
    }
}

/// UTC, like `2026-10-01T15:04:05Z`.
pub fn now() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

// ponytail: the daemon and the window may both rotate at once and lose a few lines; fine for
// a support log. Lock the file across processes if it ever matters.
fn append(path: &Path, line: &str, limit: u64) -> io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= limit) {
        std::fs::rename(path, path.with_file_name(OLD))?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(line.as_bytes())
}

#[cfg(test)]
#[test]
fn rotates() {
    let dir = std::env::temp_dir().join(format!("capralink-log-{}", crate::node::hex(&crate::node::random::<8>())));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(FILE);
    for i in 0..30 {
        append(&p, &format!("line {i}\n"), 100).unwrap();
    }
    let (cur, old) = (std::fs::read_to_string(&p).unwrap(), std::fs::read_to_string(dir.join(OLD)).unwrap());
    assert!(cur.len() < 100 && old.len() >= 100, "{cur:?} {old:?}");
    assert!(cur.ends_with("line 29\n"));
    let (last_old, first_cur) = (old.lines().last().unwrap(), cur.lines().next().unwrap());
    assert_eq!(last_old[5..].parse::<u32>().unwrap() + 1, first_cur[5..].parse::<u32>().unwrap(), "nothing lost at the rotation");
    assert_eq!(now().len(), 20);
    let _ = std::fs::remove_dir_all(dir);
}
