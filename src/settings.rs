//! Persistent user settings (`%APPDATA%\neko-player\config.json`).
//!
//! Volume / speed / loop mode / window placement are restored on startup,
//! the playlist and per-file playback positions survive restarts.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Normal (restorable) window rectangle in workspace coordinates, as
/// reported by GetWindowPlacement. `maximized` is applied on top of it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub maximized: bool,
}

/// Last known playback position of one file. The list doubles as an LRU:
/// entries are moved to the end when touched and the front is evicted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResumePos {
    pub path: String,
    pub pos: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_volume")]
    pub volume: f64,
    #[serde(default)]
    pub muted: bool,
    #[serde(default = "default_speed")]
    pub speed: f64,
    /// `LoopMode` discriminant; unknown values fall back to sequential.
    #[serde(default)]
    pub loop_mode: u8,
    #[serde(default)]
    pub playlist: Vec<String>,
    #[serde(default)]
    pub resume: Vec<ResumePos>,
    #[serde(default)]
    pub window: Option<WindowRect>,
}

fn default_volume() -> f64 {
    100.0
}

fn default_speed() -> f64 {
    1.0
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            volume: default_volume(),
            muted: false,
            speed: default_speed(),
            loop_mode: 0,
            playlist: Vec::new(),
            resume: Vec::new(),
            window: None,
        }
    }
}

/// Keep at most this many resume positions (oldest touched entries evicted).
const RESUME_CAP: usize = 50;

impl Settings {
    pub fn config_path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("neko-player").join("config.json")
    }

    pub fn load() -> Settings {
        match std::fs::read(Self::config_path()) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[neko] config parse failed, using defaults: {e}");
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("[neko] cannot create config dir: {e}");
                return;
            }
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    eprintln!("[neko] cannot write config: {e}");
                }
            }
            Err(e) => eprintln!("[neko] cannot serialize config: {e}"),
        }
    }

    /// Remember the playback position of `path` (most recent at the end).
    pub fn touch_resume(&mut self, path: &str, pos: f64) {
        if let Some(entry) = self.resume.iter_mut().find(|e| e.path == path) {
            entry.pos = pos;
            let entry = entry.clone();
            self.resume.retain(|e| e.path != path);
            self.resume.push(entry);
        } else {
            self.resume.push(ResumePos {
                path: path.to_owned(),
                pos,
            });
        }
        while self.resume.len() > RESUME_CAP {
            self.resume.remove(0);
        }
    }

    /// Saved position for `path`, if resuming makes sense (long enough ago
    /// into the file and not basically finished).
    pub fn resume_position(&self, path: &str, duration: f64) -> Option<f64> {
        let pos = self.resume.iter().find(|e| e.path == path)?.pos;
        let near_end = duration.is_finite() && duration > 0.0 && pos > duration - 20.0;
        (pos > 10.0 && !near_end).then_some(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_resume_moves_entry_to_end_and_caps_size() {
        let mut s = Settings::default();
        for i in 0..60 {
            s.touch_resume(&format!("f{i}"), 30.0);
        }
        assert_eq!(s.resume.len(), 50);
        // Oldest entries evicted, f59 most recent.
        assert_eq!(s.resume.first().map(|e| e.path.as_str()), Some("f10"));
        assert_eq!(s.resume.last().map(|e| e.path.as_str()), Some("f59"));

        s.touch_resume("f20", 55.0); // touched old entry moves to the end
        assert_eq!(s.resume.last().map(|e| e.path.as_str()), Some("f20"));
        assert_eq!(s.resume.last().map(|e| e.pos), Some(55.0));
    }

    #[test]
    fn resume_position_gates() {
        let mut s = Settings::default();
        s.touch_resume("a", 5.0);
        s.touch_resume("b", 60.0);
        s.touch_resume("c", 95.0);
        assert_eq!(s.resume_position("a", 100.0), None); // too early
        assert_eq!(s.resume_position("b", 100.0), Some(60.0));
        assert_eq!(s.resume_position("c", 100.0), None); // within 20s of end
        assert_eq!(s.resume_position("missing", 100.0), None);
    }

    #[test]
    fn defaults_used_for_partial_json() {
        let s: Settings = serde_json::from_str(r#"{"volume":42}"#).unwrap();
        assert_eq!(s.volume, 42.0);
        assert_eq!(s.speed, 1.0);
        assert!(s.playlist.is_empty());
        assert_eq!(s.loop_mode, 0);
    }
}
