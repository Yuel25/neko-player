// Release builds are pure GUI apps (no console window when double-clicked).
// Debug builds keep the console so `cargo run` still shows log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ffi;
mod player;
mod video_gl;
mod win32;

use player::{MpvPlayer, TrackKind};
use raw_window_handle::HasWindowHandle;
use raw_window_handle::RawWindowHandle;
use slint::ComponentHandle;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

slint::include_modules!();

const SPEED_STEPS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
const CONTROLS_IDLE_MS: u64 = 2500;

fn main() {
    // Pin an OpenGL(ES) backend before any component exists, so that we can
    // share Slint's GL context with libmpv's render API.
    slint::BackendSelector::new()
        .require_opengl_es()
        .select()
        .expect("failed to select an OpenGL backend for Slint");

    let ui = MainWindow::new().expect("failed to create main window");
    // The native window (and its handle) only exists once the backend
    // starts rendering; the borderless chrome is applied in RenderingSetup.
    let player = Arc::new(MpvPlayer::new());

    // --- mpv events -> Slint event loop -> UI properties ---
    {
        let player_handle = player.clone();
        let weak_handle = ui.as_weak();
        player.set_wakeup_handler(move || {
            let player = player_handle.clone();
            let weak = weak_handle.clone();
            let _ = slint::invoke_from_event_loop(move || {
                player.drain_events();
                let Some(ui) = weak.upgrade() else { return };
                let st = player.state.lock().unwrap();
                ui.set_progress(
                    if st.duration.is_finite() && st.duration > 0.0 && st.position.is_finite() {
                        (st.position / st.duration).clamp(0.0, 1.0) as f32
                    } else {
                        0.0
                    },
                );
                ui.set_time_text(fmt_time(st.position));
                ui.set_duration_text(fmt_time(st.duration));
                ui.set_paused(st.paused);
                ui.set_has_media(!st.idle);
                ui.set_media_title(st.title.clone().into());
                ui.set_volume(st.volume as f32);
                ui.set_muted(st.muted);
                ui.set_speed_text(format!("{:.2}x", st.speed).into());
                ui.set_audio_label(track_label(&st.tracks, TrackKind::Audio).into());
                ui.set_sub_label(track_label(&st.tracks, TrackKind::Subtitle).into());
                ui.set_lyric_text(st.lyric.clone().into());
                drop(st);
                ui.window().request_redraw();
            });
        });
    }

    // --- GL rendering integration (video frames into the UI) ---
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let mut renderer_slot: Option<video_gl::VideoRenderer> = None;

        let _ = ui.window().set_rendering_notifier(move |state, api| {
            match state {
                slint::RenderingState::RenderingSetup => {
                    // Strip the system caption but keep the resize frame;
                    // our Slint title bar replaces it.
                    if let Some(ui) = weak.upgrade() {
                        if let Some(hwnd) = native_hwnd(&ui) {
                            win32::apply_borderless_chrome(hwnd);
                            // Dropping files anywhere on the window plays them;
                            // WM_DROPFILES is dispatched on this UI thread.
                            let drop_player = player.clone();
                            win32::install_file_drop(
                                hwnd,
                                Box::new(move |files| {
                                    if let Some(path) = files.into_iter().next() {
                                        drop_player.load_file(&path);
                                    }
                                }),
                            );
                            eprintln!("[neko] borderless chrome applied");
                        } else {
                            eprintln!("[neko] no native window handle; keeping system frame");
                        }
                    }

                    let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
                        eprintln!("[neko] non-OpenGL graphics API; video disabled");
                        return;
                    };
                    video_gl::install_loader(*get_proc_address);
                    let gl =
                        unsafe { glow::Context::from_loader_function_cstr(video_gl::get_proc) };

                    // Ask Slint to repaint whenever mpv has a new frame.
                    let weak_cb = weak.clone();
                    let player_cb = player.clone();
                    let renderer = video_gl::VideoRenderer::new(gl, player_cb, move || {
                        let weak = weak_cb.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.window().request_redraw();
                            }
                        });
                    });

                    if let Some(ui) = weak.upgrade() {
                        ui.set_video_frame(renderer.current_image());
                    }
                    renderer_slot = Some(renderer);
                    // Now that the render context exists it is safe to load
                    // files (vo=libmpv fails fatally without one).
                    player.mark_render_ready();
                }
                slint::RenderingState::BeforeRendering => {
                    if let Some(r) = renderer_slot.as_mut() {
                        if let Some(img) = r.render_frame() {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_video_frame(img);
                            }
                        }
                    }
                }
                slint::RenderingState::RenderingTeardown => {
                    if let Some(mut r) = renderer_slot.take() {
                        r.teardown();
                    }
                }
                _ => {}
            }
        });
    }

    // --- UI callbacks ---
    // Commands also count as activity so the control bar never hides while
    // the user is interacting with it.
    let keep_active = |ui: &MainWindow, timer: &slint::Timer, player: &Arc<MpvPlayer>| {
        ui.set_controls_visible(true);
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(CONTROLS_IDLE_MS),
            {
                let player = player.clone();
                let weak = ui.as_weak();
                move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let playing = !player.state.lock().unwrap().paused;
                    if playing && !ui.get_bar_hover() {
                        ui.set_controls_visible(false);
                    }
                }
            },
        );
    };

    let activity_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_notify_activity(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
        });
    }

    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_play_pause(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.command("cycle pause");
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_seek(move |frac| {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.seek_fraction(frac as f64);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_seek_by(move |sec| {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.seek_relative(sec as f64);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_step_frame(move |direction| {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.step_frame(direction);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_cycle_audio(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.cycle_audio();
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_cycle_sub(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.cycle_sub();
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_cycle_speed(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            let cur = player.state.lock().unwrap().speed;
            let idx = SPEED_STEPS
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| (**a - cur).abs().total_cmp(&(**b - cur).abs()))
                .map(|(i, _)| i)
                .unwrap_or(2);
            player.set_speed(SPEED_STEPS[(idx + 1) % SPEED_STEPS.len()]);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_toggle_mute(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.command("cycle mute");
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_set_volume(move |vol| {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.set_volume(vol as f64);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_volume_by(move |delta| {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
            }
            player.volume_by(delta as f64);
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_toggle_fullscreen(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
                let win = ui.window();
                let fullscreen = !win.is_fullscreen();
                win.set_fullscreen(fullscreen);
                ui.set_fullscreen(fullscreen);
                if !fullscreen {
                    // winit restores the decorated style after leaving
                    // fullscreen; re-strip the caption.
                    if let Some(hwnd) = native_hwnd(&ui) {
                        win32::apply_borderless_chrome(hwnd);
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_titlebar_drag(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_controls_visible(true);
                if let Some(hwnd) = native_hwnd(&ui) {
                    win32::begin_system_drag(hwnd);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_minimize_window(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                let win = ui.window();
                let maximized = !win.is_maximized();
                win.set_maximized(maximized);
                ui.set_win_maximized(maximized);
            }
        });
    }
    {
        ui.on_close_window(move || {
            slint::quit_event_loop().ok();
        });
    }
    {
        let player = player.clone();
        ui.on_open_file(move || {
            let dlg = rfd::FileDialog::new().set_title("打开媒体文件").add_filter(
                "媒体文件",
                &[
                    "mp4", "mkv", "webm", "avi", "mov", "flv", "ts", "m2ts", "mp3", "flac", "ogg",
                    "wav", "opus", "m4a",
                ],
            );
            if let Some(path) = dlg.pick_file() {
                player.load_file(&path);
            }
        });
    }
    {
        let player = player.clone();
        ui.on_save_current_frame(move || {
            let unix_seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let dlg = rfd::FileDialog::new()
                .set_title("保存当前帧")
                .set_file_name(format!("neko-frame-{unix_seconds}.png"))
                .add_filter("PNG 图片", &["png"]);
            if let Some(path) = dlg.save_file() {
                player.save_current_frame(&path);
            }
        });
    }

    if let Some(file) = std::env::args().nth(1) {
        player.load_file(std::path::Path::new(&file));
    }

    // Kick the idle timer once so the bar hides during unattended playback.
    keep_active(&ui, &activity_timer, &player);

    let _ = ui.run();

    player.shutdown();
}

fn native_hwnd(ui: &MainWindow) -> Option<*mut c_void> {
    let wh = ui.window().window_handle();
    let rwh = wh.window_handle().ok()?;
    match rwh.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut c_void),
        _ => None,
    }
}

fn fmt_time(t: f64) -> slint::SharedString {
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

fn track_label(tracks: &[player::TrackInfo], kind: TrackKind) -> String {
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
