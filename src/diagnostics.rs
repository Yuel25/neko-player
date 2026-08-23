//! Startup and graphics diagnostics written before the Slint UI is available.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("neko-player").join("diagnostics.log")
}

pub fn start_session() {
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = File::create(path) {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unknown: {e}>"));
        let _ = writeln!(
            file,
            "neko player {} graphics diagnostics",
            env!("CARGO_PKG_VERSION")
        );
        let _ = writeln!(file, "executable: {exe}");
        let _ = writeln!(file, "os: {}", std::env::consts::OS);
        let _ = writeln!(file, "arch: {}", std::env::consts::ARCH);
        let _ = writeln!(
            file,
            "processor: {}",
            std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
        );
    }
    log("startup", "diagnostics session started");
}

pub fn log(stage: &str, message: impl AsRef<str>) {
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let message = message.as_ref().replace('\r', " ").replace('\n', " | ");
        let _ = writeln!(file, "[{stamp:.3}] {stage}: {message}");
    }
}
