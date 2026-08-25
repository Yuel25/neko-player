use crate::{diagnostics, player, settings, win32, MainWindow, PlayItem};
use player::{MpvPlayer, TrackKind};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub(crate) fn native_hwnd(ui: &MainWindow) -> Option<*mut c_void> {
    let wh = ui.window().window_handle();
    let rwh = wh.window_handle().ok()?;
    match rwh.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut c_void),
        _ => None,
    }
}

pub(crate) fn media_dialog() -> rfd::FileDialog {
    rfd::FileDialog::new().set_title("选择媒体文件").add_filter(
        "媒体文件",
        &[
            "mp4", "mkv", "webm", "avi", "mov", "flv", "ts", "m2ts", "mp3", "flac", "ogg", "wav",
            "opus", "m4a",
        ],
    )
}

pub(crate) fn sync_playlist_ui(player: &MpvPlayer, ui: &MainWindow) {
    if player.take_playlist_dirty() {
        let titles: Vec<PlayItem> = player
            .state
            .lock()
            .unwrap()
            .playlist
            .iter()
            .map(|e| PlayItem {
                title: e.title.clone().into(),
            })
            .collect();
        ui.set_playlist_model(Rc::new(slint::VecModel::from(titles)).into());
    }
    let st = player.state.lock().unwrap();
    ui.set_playlist_current(st.current_index.map_or(-1, |i| i as i32));
    ui.set_playlist_count(st.playlist.len() as i32);
    ui.set_loop_label(st.loop_mode.label().into());
}

pub(crate) fn snapshot_and_save(
    ui: &MainWindow,
    player: &MpvPlayer,
    settings_rc: &Arc<Mutex<settings::Settings>>,
) {
    {
        let st = player.state.lock().unwrap();
        let mut cfg = settings_rc.lock().unwrap();
        cfg.volume = st.volume;
        cfg.muted = st.muted;
        cfg.speed = st.speed;
        cfg.loop_mode = st.loop_mode.as_u8();
        cfg.playlist = st
            .playlist
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
    }
    if let Some(hwnd) = native_hwnd(ui) {
        if let Some(r) = win32::save_window_rect(hwnd) {
            settings_rc.lock().unwrap().window = Some(settings::WindowRect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                maximized: r.maximized,
            });
        }
    }
    match settings_rc.lock().unwrap().save() {
        Ok(()) => eprintln!(
            "[neko] settings saved to {}",
            settings::Settings::config_path().display()
        ),
        Err(e) => {
            diagnostics::log("settings", format!("save failed: {e}"));
            eprintln!("[neko] settings save failed: {e}");
        }
    }
}

pub(crate) fn fmt_time(t: f64) -> slint::SharedString {
    if !t.is_finite() || t < 0.0 {
        return "00:00".into();
    }
    let total = t as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}").into()
    } else {
        format!("{m:02}:{s:02}").into()
    }
}

pub(crate) fn track_label(tracks: &[player::TrackInfo], kind: TrackKind) -> String {
    let prefix = match kind {
        TrackKind::Audio => "音轨",
        _ => "字幕",
    };
    let list: Vec<&player::TrackInfo> = tracks.iter().filter(|t| t.kind == kind).collect();
    if list.is_empty() {
        return format!("{prefix} 无");
    }
    let n = list.len();
    match list.iter().position(|t| t.selected) {
        Some(i) => format!("{prefix} {}/{} {}", i + 1, n, list[i].label),
        None => format!("{prefix} 关"),
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_time, track_label};
    use crate::player::{TrackInfo, TrackKind};

    fn track(kind: TrackKind, selected: bool, label: &str) -> TrackInfo {
        TrackInfo {
            id: 1,
            kind,
            selected,
            label: label.to_owned(),
        }
    }

    #[test]
    fn formats_media_times() {
        assert_eq!(fmt_time(0.0).as_str(), "00:00");
        assert_eq!(fmt_time(65.9).as_str(), "01:05");
        assert_eq!(fmt_time(3661.0).as_str(), "1:01:01");
        assert_eq!(fmt_time(f64::NAN).as_str(), "00:00");
        assert_eq!(fmt_time(f64::INFINITY).as_str(), "00:00");
        assert_eq!(fmt_time(-1.0).as_str(), "00:00");
    }

    #[test]
    fn formats_track_labels() {
        let tracks = vec![
            track(TrackKind::Audio, false, "日语"),
            track(TrackKind::Audio, true, "英语"),
            track(TrackKind::Subtitle, false, "中文"),
        ];
        assert_eq!(track_label(&tracks, TrackKind::Audio), "音轨 2/2 英语");
        assert_eq!(track_label(&tracks, TrackKind::Subtitle), "字幕 关");
        assert_eq!(track_label(&tracks, TrackKind::Video), "字幕 无");
    }
}
