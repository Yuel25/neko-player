// Release builds are pure GUI apps (no console window when double-clicked).
// Debug builds keep the console so `cargo run` still shows log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_support;
mod diagnostics;
mod ffi;
mod player;
mod settings;
mod thumb;
mod video_gl;
mod win32;

use app_support::{
    fmt_time, media_dialog, native_hwnd, snapshot_and_save, sync_playlist_ui, track_label,
};
use glow::HasContext;
use player::{LoopMode, MpvPlayer, TrackKind};
use slint::ComponentHandle;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

slint::include_modules!();

const SPEED_STEPS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
const CONTROLS_IDLE_MS: u64 = 2500;

fn main() {
    diagnostics::start_session();
    diagnostics::log("backend", "requesting Slint OpenGL ES backend");

    // Pin an OpenGL(ES) backend before any component exists, so that we can
    // share Slint's GL context with libmpv's render API.
    if let Err(e) = slint::BackendSelector::new().require_opengl_es().select() {
        let detail = format!("Slint OpenGL ES backend selection failed: {e}");
        diagnostics::log("fatal", &detail);
        win32::show_error(&format!(
            "无法初始化图形后端，neko player 不能启动。\n\n建议更新显卡驱动，然后重新运行。\n\n错误：{detail}\n\n诊断日志：{}",
            diagnostics::path().display()
        ));
        return;
    }
    diagnostics::log("backend", "Slint OpenGL ES backend selected");

    let ui = match MainWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            let detail = format!("main window creation failed: {e}");
            diagnostics::log("fatal", &detail);
            win32::show_error(&format!(
                "无法创建播放器窗口。\n\n建议更新显卡驱动，然后重新运行。\n\n错误：{detail}\n\n诊断日志：{}",
                diagnostics::path().display()
            ));
            return;
        }
    };
    // The native window (and its handle) only exists once the backend
    // starts rendering; the borderless chrome is applied in RenderingSetup.
    let player = Arc::new(MpvPlayer::new());

    // Persistent settings: restore playback prefs and the last playlist.
    let settings_rc = Arc::new(std::sync::Mutex::new(settings::Settings::load()));
    {
        let cfg = settings_rc.lock().unwrap();
        player.set_volume(cfg.volume);
        if cfg.muted {
            player.command("set mute yes");
        }
        player.set_speed(cfg.speed);
        player.set_loop_mode(LoopMode::from_u8(cfg.loop_mode));
        if !cfg.playlist.is_empty() {
            let paths: Vec<std::path::PathBuf> =
                cfg.playlist.iter().map(|p| p.as_str().into()).collect();
            player.init_playlist(paths);
        }
    }
    // Position offered by the resume-playback prompt.
    let resume_pos = Arc::new(std::sync::Mutex::new(None::<f64>));

    // Hover-thumbnail preview (second headless mpv, encode-to-rawvideo).
    let thumbnailer = {
        let dir = std::env::temp_dir().join(format!("neko-player-thumbs-{}", std::process::id()));
        match thumb::Thumbnailer::new(dir) {
            Ok(t) => Some(Arc::new(t)),
            Err(e) => {
                eprintln!("[neko] progress preview disabled: {e}");
                None
            }
        }
    };
    // Cache key of the thumbnail the cursor currently wants; late results
    // for other keys are dropped.
    let preview_wanted: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));

    // --- mpv events -> Slint event loop -> UI properties ---
    {
        let player_handle = player.clone();
        let weak_handle = ui.as_weak();
        let settings_sync = settings_rc.clone();
        let resume_sync = resume_pos.clone();
        let preview_wanted_sync = preview_wanted.clone();
        player.set_wakeup_handler(move || {
            let player = player_handle.clone();
            let weak = weak_handle.clone();
            let settings_rc = settings_sync.clone();
            let resume_pos = resume_sync.clone();
            let preview_wanted_sync = preview_wanted_sync.clone();
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
                // Continuously remember where the current file is playing,
                // so it can be offered next time the file is opened.
                let touch = (!st.idle
                    && st.position > 3.0
                    && (st.duration <= 0.0 || st.position < st.duration - 5.0))
                    .then(|| {
                        st.media_path
                            .as_ref()
                            .and_then(|p| p.to_str())
                            .map(|s| (s.to_owned(), st.position))
                    })
                    .flatten();
                drop(st);
                if let Some((path, pos)) = touch {
                    settings_rc.lock().unwrap().touch_resume(&path, pos);
                }
                sync_playlist_ui(&player, &ui);
                // After a file finished loading, offer to resume it.
                if let Some(path) = player.take_file_loaded() {
                    ui.set_resume_visible(false);
                    *preview_wanted_sync.lock().unwrap() = None;
                    ui.set_preview_frame(slint::Image::default());
                    let duration = player.state.lock().unwrap().duration;
                    let path = path.to_string_lossy().into_owned();
                    if let Some(pos) = settings_rc.lock().unwrap().resume_position(&path, duration)
                    {
                        *resume_pos.lock().unwrap() = Some(pos);
                        ui.set_resume_text(fmt_time(pos));
                        ui.set_resume_visible(true);
                    }
                }
                ui.window().request_redraw();
            });
        });
    }

    // --- thumbnail results -> UI ---
    if let Some(t) = thumbnailer.clone() {
        let weak = ui.as_weak();
        let wanted = preview_wanted.clone();
        t.set_result_handler(move |key, img| {
            if wanted.lock().unwrap().as_deref() != Some(key) {
                return; // stale result, the cursor moved on
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_preview_frame(img);
            }
        });
    }

    // --- GL rendering integration (video frames into the UI) ---
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let settings_init = settings_rc.clone();
        let mut renderer_slot: Option<video_gl::VideoRenderer> = None;

        let _ = ui.window().set_rendering_notifier(move |state, api| {
            match state {
                slint::RenderingState::RenderingSetup => {
                    // Strip the system caption but keep the resize frame;
                    // our Slint title bar replaces it.
                    if let Some(ui) = weak.upgrade() {
                        if let Some(hwnd) = native_hwnd(&ui) {
                            win32::apply_borderless_chrome(hwnd);
                            // Restore the saved window placement before the
                            // first frame becomes visible.
                            if let Some(r) = settings_init.lock().unwrap().window {
                                if win32::restore_window_rect(
                                    hwnd,
                                    win32::SavedRect {
                                        x: r.x,
                                        y: r.y,
                                        w: r.w,
                                        h: r.h,
                                        maximized: r.maximized,
                                    },
                                ) && r.maximized
                                {
                                    ui.set_win_maximized(true);
                                }
                            }
                            // Dropping files anywhere on the window plays them;
                            // WM_DROPFILES is dispatched on this UI thread.
                            let drop_player = player.clone();
                            win32::install_file_drop(
                                hwnd,
                                Box::new(move |files| {
                                    drop_player.drop_files(&files);
                                }),
                            );
                            eprintln!("[neko] borderless chrome applied");
                        } else {
                            eprintln!("[neko] no native window handle; keeping system frame");
                        }
                    }

                    let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
                        let detail = format!("unexpected Slint graphics API: {api:?}");
                        diagnostics::log("fatal", &detail);
                        win32::show_error(&format!(
                            "播放器没有获得 OpenGL 图形后端，视频渲染已禁用。\n\n错误：{detail}\n\n诊断日志：{}",
                            diagnostics::path().display()
                        ));
                        return;
                    };
                    video_gl::install_loader(*get_proc_address);
                    let gl = unsafe {
                        glow::Context::from_loader_function_cstr(video_gl::get_proc)
                    };
                    let gl_info = unsafe {
                        format!(
                            "vendor={} | renderer={} | version={} | shading_language={}",
                            gl.get_parameter_string(glow::VENDOR),
                            gl.get_parameter_string(glow::RENDERER),
                            gl.get_parameter_string(glow::VERSION),
                            gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
                        )
                    };
                    diagnostics::log("opengl", gl_info);

                    let weak_cb = weak.clone();
                    let player_cb = player.clone();
                    let renderer = match video_gl::VideoRenderer::new(gl, player_cb, move || {
                        let weak = weak_cb.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.window().request_redraw();
                            }
                        });
                    }) {
                        Ok(renderer) => renderer,
                        Err(e) => {
                            diagnostics::log("mpv-render", &e);
                            win32::show_error(&format!(
                                "OpenGL 已初始化，但 libmpv 视频渲染器创建失败。\n音频和界面仍可使用。\n\n错误：{e}\n\n诊断日志：{}",
                                diagnostics::path().display()
                            ));
                            return;
                        }
                    };
                    diagnostics::log("mpv-render", "render context created");

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
                ui.set_resume_visible(false);
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
                ui.set_resume_visible(false);
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
    let config_saved = Rc::new(Cell::new(false));
    {
        let player_for_close = player.clone();
        let settings_close = settings_rc.clone();
        let weak = ui.as_weak();
        let saved_flag = config_saved.clone();
        ui.on_close_window(move || {
            if let Some(ui) = weak.upgrade() {
                snapshot_and_save(&ui, &player_for_close, &settings_close);
            }
            saved_flag.set(true);
            slint::quit_event_loop().ok();
        });
    }
    // Alt+F4 / system-menu close: save while the native window still exists,
    // otherwise its placement can no longer be queried after run() returns.
    {
        let player_for_close = player.clone();
        let settings_close = settings_rc.clone();
        let weak = ui.as_weak();
        let saved_flag = config_saved.clone();
        ui.window().on_close_requested(move || {
            if !saved_flag.get() {
                if let Some(ui) = weak.upgrade() {
                    snapshot_and_save(&ui, &player_for_close, &settings_close);
                }
                saved_flag.set(true);
            }
            slint::quit_event_loop().ok();
            slint::CloseRequestResponse::HideWindow
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_prev_track(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
                ui.set_resume_visible(false);
            }
            player.skip_prev();
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let timer = activity_timer.clone();
        ui.on_next_track(move || {
            if let Some(ui) = weak.upgrade() {
                keep_active(&ui, &timer, &player);
                ui.set_resume_visible(false);
            }
            player.skip_next();
        });
    }
    {
        let player = player.clone();
        ui.on_play_index(move |idx| {
            if idx >= 0 {
                player.play_index(idx as usize);
            }
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        ui.on_remove_index(move |idx| {
            if idx >= 0 {
                player.remove_index(idx as usize);
                if let Some(ui) = weak.upgrade() {
                    sync_playlist_ui(&player, &ui);
                }
            }
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        ui.on_clear_playlist(move || {
            player.clear_playlist();
            if let Some(ui) = weak.upgrade() {
                sync_playlist_ui(&player, &ui);
            }
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        ui.on_cycle_loop(move || {
            player.cycle_loop_mode();
            if let Some(ui) = weak.upgrade() {
                sync_playlist_ui(&player, &ui);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_playlist(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_playlist_visible(!ui.get_playlist_visible());
            }
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        ui.on_add_file(move || {
            if let Some(paths) = media_dialog().pick_files() {
                player.add_files(&paths);
                if let Some(ui) = weak.upgrade() {
                    sync_playlist_ui(&player, &ui);
                }
            }
        });
    }
    {
        let player = player.clone();
        let weak = ui.as_weak();
        let resume_pos = resume_pos.clone();
        ui.on_resume_accept(move || {
            if let Some(ui) = weak.upgrade() {
                if let Some(t) = *resume_pos.lock().unwrap() {
                    player.command(&format!("no-osd seek {t:.3} absolute"));
                }
                ui.set_resume_visible(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_resume_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_resume_visible(false);
            }
        });
    }
    {
        let player = player.clone();
        let thumbnailer_for_hover = thumbnailer.clone();
        let weak = ui.as_weak();
        let wanted = preview_wanted.clone();
        ui.on_preview_activity(move |hover, frac| {
            let Some(ui) = weak.upgrade() else { return };
            if !hover {
                *wanted.lock().unwrap() = None;
                if let Some(t) = thumbnailer_for_hover.as_ref() {
                    t.cancel_queued();
                }
                ui.set_preview_frame(slint::Image::default());
                return;
            }
            let (duration, path, video_w, video_h) = {
                let st = player.state.lock().unwrap();
                (st.duration, st.media_path.clone(), st.video_w, st.video_h)
            };
            let Some(path) = path else { return };
            if !duration.is_finite() || duration <= 0.0 {
                return;
            }
            let time = (f64::from(frac).clamp(0.0, 1.0) * duration).max(0.0);
            ui.set_preview_text(fmt_time(time));
            if video_w <= 0 || video_h <= 0 {
                *wanted.lock().unwrap() = None;
                ui.set_preview_frame(slint::Image::default());
                return; // audio-only: popup shows just the time
            }
            let Some(t) = thumbnailer_for_hover.as_ref() else {
                return;
            };
            let (key, cached) = t.request(&path, time, duration, video_w, video_h);
            *wanted.lock().unwrap() = Some(key);
            if let Some(img) = cached {
                ui.set_preview_frame(img);
            } else {
                ui.set_preview_frame(slint::Image::default());
            }
        });
    }
    {
        let player = player.clone();
        ui.on_open_file(move || {
            if let Some(paths) = media_dialog().pick_files() {
                player.play_files(&paths);
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

    let files: Vec<std::path::PathBuf> = std::env::args()
        .skip(1)
        .map(std::path::PathBuf::from)
        .collect();
    if !files.is_empty() {
        player.play_files(&files);
    }

    // Kick the idle timer once so the bar hides during unattended playback.
    keep_active(&ui, &activity_timer, &player);

    // With no media loading there may be no mpv event to trigger the wakeup
    // sync; push the restored playlist into the UI once up front.
    sync_playlist_ui(&player, &ui);

    let _ = ui.run();

    // Covers exits that bypass the close button (e.g. Alt+F4); snapshot_and_
    // save is idempotent via the saved flag.
    if !config_saved.get() {
        snapshot_and_save(&ui, &player, &settings_rc);
    }

    if let Some(t) = thumbnailer.as_ref() {
        t.shutdown();
    }
    player.shutdown();
}
