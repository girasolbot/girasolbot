use log::{Log, Metadata, Record, Level, LevelFilter};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
const MAX_OLD_FILES: u32 = 3;

struct DualLogger {
    log_file: Mutex<File>,
    log_path: PathBuf,
    level: LevelFilter,
}

impl Log for DualLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let level = record.level();
        let module = record.module_path().unwrap_or("unknown");
        let line = format!("{} [{:<5}] [{}] {}\n", now, level, module, record.args());

        // Write to stdout (with color)
        let colored = match level {
            Level::Error => format!("\x1b[31m{}\x1b[0m", line), // red
            Level::Warn  => format!("\x1b[33m{}\x1b[0m", line), // yellow
            Level::Info  => line.clone(),
            Level::Debug => format!("\x1b[36m{}\x1b[0m", line), // cyan
            Level::Trace => format!("\x1b[90m{}\x1b[0m", line), // gray
        };
        let _ = std::io::stdout().write_all(colored.as_bytes());

        // Write to file (no color)
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();

            // Check size and rotate if needed
            if let Ok(meta) = f.metadata() {
                if meta.len() > MAX_LOG_SIZE {
                    if let Some(rotated) = rotate_log_file(&self.log_path) {
                        *f = rotated;
                    }
                }
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.log_file.lock() {
            let _ = f.flush();
        }
    }
}

/// Rotate log files: sniper.log → sniper.log.1, .1 → .2, .2 → .3, delete .3+
fn rotate_log_file(log_path: &Path) -> Option<File> {
    let base = log_path.to_string_lossy().to_string();

    // Shift old files: .3 is deleted, .2→.3, .1→.2
    for i in (1..MAX_OLD_FILES).rev() {
        let src = format!("{}.{}", base, i);
        let dst = format!("{}.{}", base, i + 1);
        if Path::new(&src).exists() {
            let _ = fs::rename(&src, &dst);
        }
    }

    // Current → .1
    let backup = format!("{}.1", base);
    let _ = fs::rename(log_path, &backup);

    // Open fresh file
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path)
        .ok()
}

/// Initialize dual logging (stdout + file). Call once at startup.
pub fn init() {
    let log_dir = Path::new("logs");
    if !log_dir.exists() {
        let _ = fs::create_dir_all(log_dir);
    }

    let log_path = log_dir.join("sniper.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("Failed to open log file logs/sniper.log");

    // Respect RUST_LOG env var, default to info
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);

    let logger = DualLogger {
        log_file: Mutex::new(file),
        log_path,
        level,
    };

    log::set_boxed_logger(Box::new(logger))
        .expect("Failed to set logger");
    log::set_max_level(level);
}
