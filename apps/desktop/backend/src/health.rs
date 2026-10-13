//! Crash containment: every panic is recorded (message, file:line, thread)
//! so Diagnostics can show it, and optionally appended to a log file.
//! The host recovers a poisoned engine lock instead of failing every later
//! command ("shell lock poisoned").

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static PANICS: AtomicU64 = AtomicU64::new(0);
static LAST: Mutex<Option<String>> = Mutex::new(None);
static LOG: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// Installs the recording hook (keeps the default hook's stderr output).
/// `log_file`: where to append panic reports (e.g. the app data dir).
pub fn install_panic_hook(log_file: Option<std::path::PathBuf>) {
    if let Ok(mut l) = LOG.lock() {
        *l = log_file;
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let line = format!("panic in '{thread}' at {at}: {msg}");
        record(&line);
        default(info);
    }));
}

/// Records an internal failure (also used for recovered lock poisoning).
pub fn record(line: &str) {
    PANICS.fetch_add(1, Ordering::Relaxed);
    let line: String = line.chars().take(600).collect();
    if let Ok(mut l) = LAST.lock() {
        *l = Some(line.clone());
    }
    let path = LOG.lock().ok().and_then(|p| p.clone());
    if let Some(path) = path {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{ts}] {line}");
        }
    }
}

/// (internal failures since start, last one).
pub fn status() -> (u64, Option<String>) {
    (PANICS.load(Ordering::Relaxed), LAST.lock().ok().and_then(|l| l.clone()))
}

/// Runs `f`, turning a panic into `None` (already recorded by the hook).
pub fn contain<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn contained_panic_is_recorded() {
        super::install_panic_hook(None);
        let before = super::status().0;
        let r = super::contain(|| -> u32 { panic!("boom-test") });
        assert!(r.is_none());
        let (n, last) = super::status();
        assert!(n > before);
        assert!(last.unwrap().contains("boom-test"));
    }
}
