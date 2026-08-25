//! Safe-ish wrapper around the libmpv client API.

use crate::ffi;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

/// One entry of mpv's `track-list` property.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    /// Will be used for explicit track selection (`set sid=<id>`).
    #[allow(dead_code)]
    pub id: i64,
    pub kind: TrackKind,
    pub selected: bool,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrackKind {
    #[default]
    Audio,
    Video,
    Subtitle,
}

impl TrackKind {
    fn from_type_str(s: &str) -> Option<TrackKind> {
        match s {
            "audio" => Some(TrackKind::Audio),
            "video" => Some(TrackKind::Video),
            "sub" => Some(TrackKind::Subtitle),
            _ => None,
        }
    }
}

/// One entry of the app-managed playlist. mpv itself always plays a single
/// file (`loadfile` replace); advancing, looping and shuffling are ours.
#[derive(Clone, Debug)]
pub struct PlaylistEntry {
    pub path: std::path::PathBuf,
    pub title: String,
}

/// Playback order for the app-managed playlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LoopMode {
    #[default]
    Sequential,
    RepeatAll,
    RepeatOne,
    Shuffle,
}

impl LoopMode {
    pub fn from_u8(v: u8) -> LoopMode {
        match v {
            1 => LoopMode::RepeatAll,
            2 => LoopMode::RepeatOne,
            3 => LoopMode::Shuffle,
            _ => LoopMode::Sequential,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Sequential => "顺序播放",
            LoopMode::RepeatAll => "列表循环",
            LoopMode::RepeatOne => "单曲循环",
            LoopMode::Shuffle => "随机播放",
        }
    }

    pub fn cycle(self) -> LoopMode {
        LoopMode::from_u8((self.as_u8() + 1) % 4)
    }
}

/// Playback state, updated by mpv events (on the UI thread) and read from
/// UI bindings and the render loop.
#[derive(Default)]
pub struct State {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub idle: bool,
    pub title: String,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub tracks: Vec<TrackInfo>,
    pub lyric: String,
    pub playlist: Vec<PlaylistEntry>,
    pub current_index: Option<usize>,
    pub loop_mode: LoopMode,
    /// Path of the media currently loaded / being loaded.
    pub media_path: Option<std::path::PathBuf>,
    pub lyrics: Vec<(f64, String)>,
    pub video_w: i64,
    pub video_h: i64,
    /// Set when the playlist contents / current index / loop mode changed and
    /// the UI model needs a rebuild; cleared by take_playlist_dirty().
    playlist_dirty: bool,
    /// Set on MPV_EVENT_FILE_LOADED; consumed via take_file_loaded().
    file_loaded_flag: bool,
    /// keep-open=yes pauses at the last frame instead of ending the file, so
    /// auto-advance is driven by this property instead of END_FILE events.
    /// Latched true and manually cleared once handled.
    eof_reached: bool,
    /// Set when END_FILE reports MPV_END_FILE_REASON_ERROR (unloadable file).
    load_failed: bool,
    /// Consecutive auto-advances that happened without playback ever passing
    /// 3 seconds; guards against a playlist of broken files looping forever.
    instant_advances: u32,
}

pub struct MpvPlayer {
    handle: *mut ffi::mpv_handle,
    pub state: Mutex<State>,
    pending_size: Mutex<Option<(u32, u32)>>,
    /// Files queued before the render context exists; with vo=libmpv the VO
    /// fatally fails ("No render context set") if a loadfile arrives first.
    pending_file: Mutex<Option<std::path::PathBuf>>,
    /// Single source of truth for the render-context lifecycle.
    /// RELEASED -> SETTING_UP -> READY -> SETTING_UP -> RELEASED.
    render_state: AtomicU8,
    /// Double-boxed wakeup closure owned until the mpv handle is destroyed.
    wakeup_ctx: Mutex<*mut c_void>,
    terminated: AtomicBool,
}

// The mpv client API is thread-safe per handle; the render context is not,
// and it is only ever touched on the UI/GL thread (see video_gl.rs).
unsafe impl Send for MpvPlayer {}
unsafe impl Sync for MpvPlayer {}

/// Hard cap for mpv `volume-max` and UI volume controls. 0-100 is the normal
/// range; above 100 mpv applies software amplification.
pub const VOLUME_MAX: f64 = 200.0;

const RENDER_RELEASED: u8 = 0;
const RENDER_SETTING_UP: u8 = 1;
const RENDER_READY: u8 = 2;

pub(crate) fn err_str(code: c_int) -> String {
    unsafe {
        let s = ffi::mpv_error_string(code);
        if s.is_null() {
            format!("mpv error {code}")
        } else {
            CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    }
}

impl MpvPlayer {
    pub fn new() -> Result<MpvPlayer, String> {
        unsafe {
            let handle = ffi::mpv_create();
            if handle.is_null() {
                return Err("mpv_create() failed".to_owned());
            }

            // vo=libmpv routes all video through the render API — without
            // it mpv briefly opens its own native player window at loadfile.
            let volume_max = VOLUME_MAX.to_string();
            for (k, v) in [
                ("vo", "libmpv"),
                ("keep-open", "yes"),
                ("hwdec", "auto-copy"),
                ("force-window", "no"),
                // Volume boost headroom (0-100% is the slider's normal range,
                // above that software amplification kicks in).
                ("volume-max", volume_max.as_str()),
                // Load same-name and reasonably matching external subtitle
                // files, and keep the selected subtitle track visible.
                ("sub-auto", "fuzzy"),
                ("sub-visibility", "yes"),
                ("osd-on-seek", "no"),
            ] {
                let ck = CString::new(k).unwrap();
                let cv = CString::new(v).unwrap();
                let r = ffi::mpv_set_property_string(handle, ck.as_ptr(), cv.as_ptr());
                if r < 0 {
                    ffi::mpv_terminate_destroy(handle);
                    return Err(format!("mpv option {k}={v} rejected: {}", err_str(r)));
                }
            }

            let r = ffi::mpv_initialize(handle);
            if r < 0 {
                ffi::mpv_terminate_destroy(handle);
                return Err(format!("mpv_initialize failed: {}", err_str(r)));
            }

            let player = MpvPlayer {
                handle,
                state: Mutex::new(State::default()),
                pending_size: Mutex::new(None),
                pending_file: Mutex::new(None),
                render_state: AtomicU8::new(RENDER_RELEASED),
                wakeup_ctx: Mutex::new(std::ptr::null_mut()),
                terminated: AtomicBool::new(false),
            };

            for (id, name, format) in [
                (1u64, "time-pos", ffi::MPV_FORMAT_DOUBLE),
                (2, "duration", ffi::MPV_FORMAT_DOUBLE),
                (3, "pause", ffi::MPV_FORMAT_FLAG),
                (4, "idle-active", ffi::MPV_FORMAT_FLAG),
                (5, "width", ffi::MPV_FORMAT_INT64),
                (6, "height", ffi::MPV_FORMAT_INT64),
                (7, "media-title", ffi::MPV_FORMAT_STRING),
                (8, "volume", ffi::MPV_FORMAT_DOUBLE),
                (9, "mute", ffi::MPV_FORMAT_FLAG),
                (10, "speed", ffi::MPV_FORMAT_DOUBLE),
                (11, "track-list", ffi::MPV_FORMAT_NODE),
                (12, "eof-reached", ffi::MPV_FORMAT_FLAG),
            ] {
                if let Err(e) = player.observe(id, name, format) {
                    ffi::mpv_terminate_destroy(handle);
                    return Err(e);
                }
            }
            Ok(player)
        }
    }

    fn observe(&self, id: u64, name: &str, format: c_int) -> Result<(), String> {
        let c = CString::new(name).unwrap();
        let r = unsafe { ffi::mpv_observe_property(self.handle, id, c.as_ptr(), format) };
        if r < 0 {
            Err(format!("observe_property({name}) failed: {}", err_str(r)))
        } else {
            Ok(())
        }
    }

    fn get_string(&self, name: &str) -> Option<String> {
        let c = CString::new(name).ok()?;
        let mut value: *mut c_char = std::ptr::null_mut();
        let result = unsafe {
            ffi::mpv_get_property(
                self.handle,
                c.as_ptr(),
                ffi::MPV_FORMAT_STRING,
                &mut value as *mut *mut c_char as *mut c_void,
            )
        };
        if result < 0 || value.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(value).to_string_lossy().into_owned() };
        unsafe { ffi::mpv_free(value as *mut c_void) };
        Some(text)
    }

    fn get_i64(&self, name: &str) -> Option<i64> {
        let c = CString::new(name).unwrap();
        let mut v: i64 = 0;
        let r = unsafe {
            ffi::mpv_get_property(
                self.handle,
                c.as_ptr(),
                ffi::MPV_FORMAT_INT64,
                &mut v as *mut i64 as *mut c_void,
            )
        };
        if r < 0 {
            None
        } else {
            Some(v)
        }
    }

    pub fn handle(&self) -> *mut ffi::mpv_handle {
        self.handle
    }

    /// Enter setup/teardown before touching a render context. While in this
    /// state file loads are deferred and shutdown will not destroy the handle.
    pub fn mark_render_unavailable(&self) {
        self.render_state.store(RENDER_SETTING_UP, Ordering::SeqCst);
    }

    /// Publish that no render context exists and the mpv handle is safe to destroy.
    pub fn mark_render_released(&self) {
        self.render_state.store(RENDER_RELEASED, Ordering::SeqCst);
    }

    /// Register the wakeup callback. The closure is called on an mpv-internal
    /// thread whenever new events are pending; it must not call back into
    /// libmpv (so we only bounce into the Slint event loop).
    pub fn set_wakeup_handler(&self, f: impl Fn() + Send + 'static) {
        // Double-box: the fat pointer (data + vtable) must live behind a
        // stable thin pointer we can hand to C as an opaque context.
        let mut owned_ctx = self.wakeup_ctx.lock().unwrap();
        assert!(owned_ctx.is_null(), "wakeup handler already installed");
        let boxed: Box<dyn Fn() + Send> = Box::new(f);
        let ctx = Box::into_raw(Box::new(boxed)) as *mut c_void;
        unsafe extern "C" fn trampoline(ctx: *mut c_void) {
            let f = &*(ctx as *const Box<dyn Fn() + Send>);
            f();
        }
        unsafe { ffi::mpv_set_wakeup_callback(self.handle, Some(trampoline), ctx) };
        *owned_ctx = ctx;
    }

    pub fn command(&self, cmd: &str) {
        let c = CString::new(cmd).unwrap();
        let r = unsafe { ffi::mpv_command_string(self.handle, c.as_ptr()) };
        if r < 0 {
            eprintln!("[neko] mpv command `{cmd}` failed: {}", err_str(r));
        }
    }

    pub fn load_file(&self, path: &std::path::Path) {
        let lyrics = load_lrc(path);
        {
            let mut state = self.state.lock().unwrap();
            state.lyrics = lyrics;
            state.lyric.clear();
            state.media_path = Some(path.to_path_buf());
        }
        if self.render_state.load(Ordering::SeqCst) == RENDER_READY {
            self.load_file_now(path);
        } else {
            eprintln!("[neko] deferring load until render context is up");
            *self.pending_file.lock().unwrap() = Some(path.to_path_buf());
        }
    }

    fn load_file_now(&self, path: &std::path::Path) {
        // mpv accepts forward slashes on Windows and its command parser
        // treats backslashes as escapes, so normalize first.
        let p = path.to_string_lossy().replace('\\', "/");
        let p = p.replace('"', "\\\"");
        eprintln!("[neko] loading: {p}");
        self.command(&format!("loadfile \"{p}\""));
    }

    /// Called from RenderingSetup once the mpv render context exists; any
    /// file requested earlier is loaded now.
    pub fn mark_render_ready(&self) {
        self.render_state.store(RENDER_READY, Ordering::SeqCst);
        if let Some(path) = self.pending_file.lock().unwrap().take() {
            self.load_file_now(&path);
        }
    }

    pub fn seek_fraction(&self, frac: f64) {
        let dur = self.state.lock().unwrap().duration;
        if dur > 0.0 {
            let t = (frac.clamp(0.0, 1.0) * dur).max(0.0);
            self.command(&format!("no-osd seek {t:.3} absolute"));
        }
    }

    pub fn seek_relative(&self, seconds: f64) {
        self.command(&format!("no-osd seek {seconds:+} relative+exact"));
    }

    pub fn step_frame(&self, direction: i32) {
        if direction < 0 {
            self.command("frame-back-step");
        } else {
            self.command("frame-step");
        }
    }

    pub fn save_current_frame(&self, path: &std::path::Path) {
        let path = path.to_string_lossy().replace('\\', "/");
        let path = path.replace('"', "\\\"");
        self.command(&format!("no-osd screenshot-to-file \"{path}\" video"));
    }

    // ---- playlist ----

    /// Restore a saved playlist on startup without starting playback.
    pub fn init_playlist(&self, paths: Vec<std::path::PathBuf>) {
        let mut state = self.state.lock().unwrap();
        state.playlist = paths.into_iter().map(entry_of).collect();
        state.current_index = None;
        state.playlist_dirty = true;
    }

    /// Replace the playlist with `paths` and start playing the first entry.
    pub fn play_files(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            state.playlist = paths.iter().map(|p| entry_of(p.clone())).collect();
            state.current_index = None;
            state.playlist_dirty = true;
        }
        self.play_index(0);
    }

    /// Append to the playlist. If nothing is playing, the first newly
    /// added entry starts playing.
    pub fn add_files(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let play_added = {
            let mut state = self.state.lock().unwrap();
            let start = state.playlist.len();
            state
                .playlist
                .extend(paths.iter().map(|p| entry_of(p.clone())));
            state.playlist_dirty = true;
            state.current_index.is_none().then_some(start)
        };
        if let Some(index) = play_added {
            self.play_index(index);
        }
    }

    /// Drop semantics: an empty playlist is replaced by the dropped files and
    /// starts playing; a non-empty one has them appended at the end.
    pub fn drop_files(&self, paths: &[std::path::PathBuf]) {
        let empty = self.state.lock().unwrap().playlist.is_empty();
        if empty {
            self.play_files(paths);
        } else {
            self.add_files(paths);
        }
    }

    pub fn play_index(&self, index: usize) {
        let path = {
            let mut state = self.state.lock().unwrap();
            if index >= state.playlist.len() {
                return;
            }
            state.current_index = Some(index);
            state.playlist[index].path.clone()
        };
        self.load_file(&path);
        // Selecting a track always starts it playing, even if the previous
        // one was paused at its end (keep-open leaves `pause` set).
        self.command("set pause no");
    }

    pub fn skip_next(&self) {
        let (len, cur, mode) = {
            let state = self.state.lock().unwrap();
            (state.playlist.len(), state.current_index, state.loop_mode)
        };
        let Some(cur) = cur else { return };
        if let Some(next) = manual_next_index(len, cur, mode) {
            if next == cur {
                self.replay();
            } else {
                self.play_index(next);
            }
        }
    }

    pub fn skip_prev(&self) {
        let (len, cur, mode) = {
            let state = self.state.lock().unwrap();
            (state.playlist.len(), state.current_index, state.loop_mode)
        };
        let Some(cur) = cur else { return };
        // Restart the current file first, only then go to the previous one.
        if self.state.lock().unwrap().position > 3.0 {
            self.command("no-osd seek 0 absolute");
            return;
        }
        let prev = match mode {
            LoopMode::RepeatAll => (cur + len - 1) % len,
            _ => cur.saturating_sub(1),
        };
        self.play_index(prev);
    }

    pub fn remove_index(&self, index: usize) {
        enum FollowUp {
            Nothing,
            Play(usize),
            Stop,
        }
        let follow_up = {
            let mut state = self.state.lock().unwrap();
            if index >= state.playlist.len() {
                return;
            }
            state.playlist.remove(index);
            state.playlist_dirty = true;
            match state.current_index {
                Some(i) if i > index => {
                    state.current_index = Some(i - 1);
                    FollowUp::Nothing
                }
                Some(i) if i == index => {
                    state.current_index = None;
                    if state.playlist.is_empty() {
                        FollowUp::Stop
                    } else {
                        FollowUp::Play(index.min(state.playlist.len() - 1))
                    }
                }
                _ => FollowUp::Nothing,
            }
        };
        match follow_up {
            FollowUp::Nothing => {}
            FollowUp::Play(next) => self.play_index(next),
            FollowUp::Stop => self.stop_playback(),
        }
    }

    pub fn clear_playlist(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.playlist.clear();
            state.current_index = None;
            state.playlist_dirty = true;
        }
        self.stop_playback();
    }

    pub fn cycle_loop_mode(&self) {
        let mut state = self.state.lock().unwrap();
        state.loop_mode = state.loop_mode.cycle();
    }

    pub fn set_loop_mode(&self, mode: LoopMode) {
        let mut state = self.state.lock().unwrap();
        state.loop_mode = mode;
    }

    pub fn take_playlist_dirty(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        std::mem::take(&mut state.playlist_dirty)
    }

    /// Path of the file that finished loading most recently (set on
    /// FILE_LOADED), if the UI has not consumed it yet.
    pub fn take_file_loaded(&self) -> Option<std::path::PathBuf> {
        let mut state = self.state.lock().unwrap();
        if state.file_loaded_flag {
            state.file_loaded_flag = false;
            state.media_path.clone()
        } else {
            None
        }
    }

    fn stop_playback(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.title.clear();
            state.lyric.clear();
            state.lyrics.clear();
            state.position = 0.0;
            state.duration = 0.0;
        }
        self.command("stop");
    }

    fn replay(&self) {
        self.command("no-osd seek 0 absolute");
        self.command("set pause no");
    }

    /// Advance to the next entry after the current one naturally ended
    /// (eof-reached) or failed to load. Called at the end of drain_events.
    fn advance_after_end(&self) {
        let (len, cur, mode) = {
            let state = self.state.lock().unwrap();
            (state.playlist.len(), state.current_index, state.loop_mode)
        };
        let Some(cur) = cur else { return };
        let Some(next) = auto_next_index(len, cur, mode) else {
            return; // sequential run finished; stay paused on the last frame
        };
        if next == cur {
            self.replay();
        } else {
            self.play_index(next);
        }
    }

    fn set_property(&self, name: &str, value: &str) {
        let ck = CString::new(name).unwrap();
        let cv = CString::new(value).unwrap();
        let r = unsafe { ffi::mpv_set_property_string(self.handle, ck.as_ptr(), cv.as_ptr()) };
        if r < 0 {
            eprintln!("[neko] mpv set {name}={value} failed: {}", err_str(r));
        }
    }

    pub fn set_volume(&self, vol: f64) {
        self.set_property("volume", &format!("{:.1}", vol.clamp(0.0, VOLUME_MAX)));
    }

    pub fn volume_by(&self, delta: f64) {
        let cur = self.state.lock().unwrap().volume;
        self.set_volume(cur + delta);
    }

    pub fn set_speed(&self, speed: f64) {
        self.set_property("speed", &format!("{speed:.3}"));
    }

    pub fn cycle_audio(&self) {
        self.command("cycle audio");
    }

    pub fn cycle_sub(&self) {
        let tracks: Vec<TrackInfo> = self
            .state
            .lock()
            .unwrap()
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle)
            .cloned()
            .collect();

        if tracks.is_empty() {
            return;
        }

        // Select explicitly by sid. This avoids the ambiguous `cycle sub`
        // alias and makes the sequence deterministic: off -> first -> ... -> off.
        let next = tracks
            .iter()
            .position(|t| t.selected)
            .and_then(|i| tracks.get(i + 1))
            .map(|t| t.id);
        let any_selected = tracks.iter().any(|t| t.selected);

        self.set_property("sub-visibility", "yes");
        match (any_selected, next) {
            (false, _) => self.set_property("sid", &tracks[0].id.to_string()),
            (true, Some(id)) => self.set_property("sid", &id.to_string()),
            (true, None) => self.set_property("sid", "no"),
        }
    }

    /// Consume a pending video size change (set by drain_events).
    pub fn take_pending_size(&self) -> Option<(u32, u32)> {
        self.pending_size.lock().unwrap().take()
    }

    /// Drain all pending mpv events into `state`. Must run on the UI thread
    /// (the same thread that owns the Slint window).
    pub fn drain_events(&self) {
        unsafe {
            loop {
                let ev_ptr = ffi::mpv_wait_event(self.handle, 0.0);
                if ev_ptr.is_null() {
                    break;
                }
                let ev = &*ev_ptr;
                match ev.event_id {
                    ffi::MPV_EVENT_NONE | ffi::MPV_EVENT_SHUTDOWN => break,
                    ffi::MPV_EVENT_PROPERTY_CHANGE => {
                        if ev.data.is_null() {
                            continue;
                        }
                        let prop = &*(ev.data as *const ffi::mpv_event_property);
                        if prop.name.is_null() {
                            continue;
                        }
                        let name = CStr::from_ptr(prop.name).to_string_lossy().into_owned();
                        let mut st = self.state.lock().unwrap();
                        match (name.as_str(), prop.format, prop.data as *const u8) {
                            ("time-pos", ffi::MPV_FORMAT_DOUBLE, p) if !p.is_null() => {
                                st.position = *(p as *const f64);
                                st.lyric = lyric_at(&st.lyrics, st.position).unwrap_or_default();
                                if st.position > 3.0 {
                                    st.instant_advances = 0;
                                }
                            }
                            ("duration", ffi::MPV_FORMAT_DOUBLE, p) if !p.is_null() => {
                                st.duration = *(p as *const f64);
                            }
                            ("pause", ffi::MPV_FORMAT_FLAG, p) if !p.is_null() => {
                                st.paused = *(p as *const c_int) != 0;
                            }
                            ("idle-active", ffi::MPV_FORMAT_FLAG, p) if !p.is_null() => {
                                st.idle = *(p as *const c_int) != 0;
                            }
                            ("width", ffi::MPV_FORMAT_INT64, p) if !p.is_null() => {
                                st.video_w = *(p as *const i64);
                                self.sync_video_size(&mut st);
                            }
                            ("height", ffi::MPV_FORMAT_INT64, p) if !p.is_null() => {
                                st.video_h = *(p as *const i64);
                                self.sync_video_size(&mut st);
                            }
                            ("volume", ffi::MPV_FORMAT_DOUBLE, p) if !p.is_null() => {
                                st.volume = *(p as *const f64);
                            }
                            ("mute", ffi::MPV_FORMAT_FLAG, p) if !p.is_null() => {
                                st.muted = *(p as *const c_int) != 0;
                            }
                            ("speed", ffi::MPV_FORMAT_DOUBLE, p) if !p.is_null() => {
                                st.speed = *(p as *const f64);
                            }
                            ("track-list", ffi::MPV_FORMAT_NODE, p) if !p.is_null() => {
                                // The node (and everything it references) is
                                // owned by the event and freed automatically
                                // on the next mpv_wait_event() call, so we
                                // copy everything we need right here.
                                st.tracks =
                                    node_parsing::parse_track_list(&*(p as *const ffi::mpv_node));
                            }
                            ("eof-reached", ffi::MPV_FORMAT_FLAG, p)
                                if !p.is_null() && *(p as *const c_int) != 0 =>
                            {
                                st.eof_reached = true;
                            }
                            ("media-title", ffi::MPV_FORMAT_STRING, p) if !p.is_null() => {
                                let p = p as *const *const c_char;
                                if !(*p).is_null() {
                                    st.title = CStr::from_ptr(*p).to_string_lossy().into_owned();
                                }
                            }
                            _ => {}
                        }
                    }
                    ffi::MPV_EVENT_FILE_LOADED => {
                        eprintln!("[neko] file loaded");
                        let loaded_path = self.get_string("path").map(std::path::PathBuf::from);
                        {
                            let mut state = self.state.lock().unwrap();
                            if let Some(path) = loaded_path {
                                state.media_path = Some(path);
                            }
                            state.file_loaded_flag = true;
                        }
                        // Some property changes race ahead of our observer;
                        // actively query the video size on load as well.
                        let w = self.get_i64("width");
                        let h = self.get_i64("height");
                        eprintln!(
                            "[neko] queried size: {}x{}",
                            w.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                            h.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                        );
                        if let (Some(w), Some(h)) = (w, h) {
                            let mut st = self.state.lock().unwrap();
                            st.video_w = w;
                            st.video_h = h;
                            self.sync_video_size(&mut st);
                        }
                    }
                    ffi::MPV_EVENT_END_FILE if !ev.data.is_null() => {
                        let end = &*(ev.data as *const ffi::mpv_event_end_file);
                        if end.reason == ffi::MPV_END_FILE_REASON_ERROR {
                            eprintln!("[neko] loading failed ({}), skipping", err_str(end.error));
                            self.state.lock().unwrap().load_failed = true;
                        }
                    }
                    ffi::MPV_EVENT_LOG_MESSAGE if !ev.data.is_null() => {
                        let msg = &*(ev.data as *const ffi::mpv_event_log_message);
                        let get = |p: *const c_char| {
                            if p.is_null() {
                                String::new()
                            } else {
                                CStr::from_ptr(p).to_string_lossy().into_owned()
                            }
                        };
                        let text = get(msg.text);
                        let text = text.trim_end();
                        if !text.is_empty() {
                            eprintln!("[mpv:{}:{}] {}", get(msg.prefix), get(msg.level), text);
                        }
                    }
                    _ => {}
                }
            }
        }

        // keep-open pauses at the last frame instead of ending the file, so
        // eof-reached / load errors (not END_FILE) drive auto-advance. Both
        // flags are latched in the property/event handlers above and consumed
        // here exactly once.
        let advance = {
            let mut st = self.state.lock().unwrap();
            let failed = std::mem::take(&mut st.load_failed);
            let eof = std::mem::take(&mut st.eof_reached);
            if st.current_index.is_none() {
                false
            } else if failed {
                // A chain of unloadable files would advance forever; stop
                // after the whole playlist failed to even start playing.
                if st.instant_advances > st.playlist.len() as u32 + 2 {
                    eprintln!("[neko] too many consecutive load failures; auto-advance suspended");
                    false
                } else {
                    st.instant_advances += 1;
                    true
                }
            } else {
                eof
            }
        };
        if advance {
            self.advance_after_end();
        }
    }

    fn sync_video_size(&self, st: &mut State) {
        let (w, h) = (st.video_w, st.video_h);
        if w > 0 && h > 0 {
            let clamp = |v: i64| v.clamp(1, 8192) as u32;
            *self.pending_size.lock().unwrap() = Some((clamp(w), clamp(h)));
        }
    }

    /// Destroy the mpv handle after the UI has closed. The render context
    /// must already be freed (RenderingTeardown); if it is not, we leak the
    /// handle instead of risking a use-after-free during process exit.
    pub fn shutdown(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.render_state.load(Ordering::SeqCst) == RENDER_RELEASED {
            let ctx =
                std::mem::replace(&mut *self.wakeup_ctx.lock().unwrap(), std::ptr::null_mut());
            unsafe {
                // terminate_destroy guarantees no callback can still be running;
                // reclaim its userdata only after that synchronization point.
                ffi::mpv_set_wakeup_callback(self.handle, None, std::ptr::null_mut());
                ffi::mpv_terminate_destroy(self.handle);
                if !ctx.is_null() {
                    drop(Box::from_raw(ctx as *mut Box<dyn Fn() + Send>));
                }
            }
        } else {
            eprintln!("[neko] render context was not released; leaking mpv handle on exit");
        }
    }
}

fn load_lrc(media_path: &std::path::Path) -> Vec<(f64, String)> {
    let path = media_path.with_extension("lrc");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };

    let content = if bytes.starts_with(&[0xff, 0xfe]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter(|b| b.len() == 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter(|b| b.len() == 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    } else {
        decode_legacy_lrc(&bytes)
            .trim_start_matches('\u{feff}')
            .to_owned()
    };

    let mut entries = Vec::new();
    for line in content.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(tag) = rest
            .strip_prefix('[')
            .and_then(|s| s.find(']').map(|end| (&s[..end], &s[end + 1..])))
        {
            rest = tag.1;
            let Some((minutes, seconds)) = tag.0.split_once(':') else {
                continue;
            };
            if let (Ok(minutes), Ok(seconds)) = (minutes.parse::<f64>(), seconds.parse::<f64>()) {
                times.push(minutes * 60.0 + seconds);
            }
        }
        let text = rest.trim();
        if !text.is_empty() {
            entries.extend(times.into_iter().map(|time| (time, text.to_owned())));
        }
    }
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    entries
}

fn decode_legacy_lrc(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }

    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn MultiByteToWideChar(
                code_page: u32,
                flags: u32,
                input: *const u8,
                input_len: c_int,
                output: *mut u16,
                output_len: c_int,
            ) -> c_int;
        }

        unsafe fn decode(
            bytes: &[u8],
            code_page: u32,
            convert: unsafe extern "system" fn(
                u32,
                u32,
                *const u8,
                c_int,
                *mut u16,
                c_int,
            ) -> c_int,
        ) -> Option<String> {
            let len = convert(
                code_page,
                0,
                bytes.as_ptr(),
                bytes.len() as c_int,
                std::ptr::null_mut(),
                0,
            );
            if len <= 0 {
                return None;
            }
            let mut utf16 = vec![0u16; len as usize];
            let written = convert(
                code_page,
                0,
                bytes.as_ptr(),
                bytes.len() as c_int,
                utf16.as_mut_ptr(),
                len,
            );
            (written > 0).then(|| String::from_utf16_lossy(&utf16[..written as usize]))
        }

        fn japanese_score(text: &str) -> i64 {
            text.chars()
                .map(|c| match c {
                    '\u{3040}'..='\u{30ff}' => 5,  // hiragana / full-width katakana
                    '\u{4e00}'..='\u{9fff}' => 1,  // CJK ideographs
                    '\u{ff61}'..='\u{ff9f}' => -2, // suspicious half-width mojibake
                    '\u{fffd}' => -20,
                    _ if c.is_control() && c != '\n' && c != '\r' && c != '\t' => -10,
                    _ => 0,
                })
                .sum()
        }

        // Older Japanese lyric collections occur both as Windows-31J and as
        // CP936/GBK files that still contain mapped Japanese kana. Decode both
        // and choose the candidate with the strongest valid Japanese signal.
        let candidates = [932u32, 936u32]
            .into_iter()
            .filter_map(|cp| decode(bytes, cp, MultiByteToWideChar));
        if let Some(best) = candidates.max_by_key(|text| japanese_score(text)) {
            return best;
        }
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn lyric_at(entries: &[(f64, String)], position: f64) -> Option<String> {
    let index = entries.partition_point(|(time, _)| *time <= position);
    index.checked_sub(1).map(|i| entries[i].1.clone())
}

fn entry_of(path: std::path::PathBuf) -> PlaylistEntry {
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    PlaylistEntry { path, title }
}

/// Next index when the current file ended on its own. `RepeatOne` replays
/// the same entry; callers detect `Some(cur)` and replay via seek.
fn auto_next_index(len: usize, cur: usize, mode: LoopMode) -> Option<usize> {
    match mode {
        LoopMode::RepeatOne => Some(cur),
        LoopMode::Sequential => (cur + 1 < len).then_some(cur + 1),
        LoopMode::RepeatAll => (len > 0).then(|| (cur + 1) % len),
        LoopMode::Shuffle => (len > 0).then(|| shuffle_other(len, cur)),
    }
}

/// Next index for the manual "next" button. `RepeatOne` behaves like
/// sequential (manual skips should leave the entry), and shuffle picks a
/// random *other* entry.
fn manual_next_index(len: usize, cur: usize, mode: LoopMode) -> Option<usize> {
    match mode {
        LoopMode::RepeatOne => (cur + 1 < len).then_some(cur + 1),
        LoopMode::Sequential => (cur + 1 < len).then_some(cur + 1),
        LoopMode::RepeatAll => (len > 0).then(|| (cur + 1) % len),
        LoopMode::Shuffle => (len > 0).then(|| shuffle_other(len, cur)),
    }
}

/// Uniform index in `0..len` that is not `cur` (returns `cur` for len <= 1).
fn shuffle_other(len: usize, cur: usize) -> usize {
    if len <= 1 {
        return cur;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as u64) ^ (d.as_secs() << 17))
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut state = nanos | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (cur + 1 + (state as usize % (len - 1))) % len
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    #[test]
    fn auto_advance_follows_loop_mode() {
        use LoopMode::*;
        assert_eq!(auto_next_index(3, 0, Sequential), Some(1));
        assert_eq!(auto_next_index(3, 2, Sequential), None);
        assert_eq!(auto_next_index(3, 2, RepeatAll), Some(0));
        assert_eq!(auto_next_index(1, 0, RepeatAll), Some(0));
        assert_eq!(auto_next_index(3, 1, RepeatOne), Some(1));
    }

    #[test]
    fn manual_next_leaves_repeat_one() {
        use LoopMode::*;
        assert_eq!(manual_next_index(3, 1, RepeatOne), Some(2));
        assert_eq!(manual_next_index(3, 2, RepeatOne), None);
        assert_eq!(manual_next_index(3, 2, Sequential), None);
        assert_eq!(manual_next_index(3, 2, RepeatAll), Some(0));
    }

    #[test]
    fn shuffle_never_picks_current() {
        for _ in 0..64 {
            let next = shuffle_other(5, 2);
            assert!((0..5).contains(&next) && next != 2);
        }
        assert_eq!(shuffle_other(1, 0), 0);
    }

    #[test]
    fn loop_mode_roundtrip() {
        let mut mode = LoopMode::Sequential;
        for _ in 0..4 {
            mode = mode.cycle();
        }
        assert_eq!(mode, LoopMode::Sequential);
        assert_eq!(
            LoopMode::from_u8(LoopMode::Shuffle.as_u8()),
            LoopMode::Shuffle
        );
    }
}

#[cfg(test)]
mod lyric_tests {
    use super::decode_legacy_lrc;

    #[test]
    fn detects_cp936_file_with_japanese_kana() {
        let encoded =
            b"[ti:\xd0\xc4\xa4\xc8\xa4\xa4\xa4\xa6\xc3\xfb\xa4\xce\xb2\xbb\xbf\xc9\xbd\xe2]";
        assert_eq!(decode_legacy_lrc(encoded), "[ti:心という名の不可解]");
    }
}

mod node_parsing {
    use super::*;
    use crate::ffi;

    /// Parse an mpv NODE_ARRAY of NODE_MAPs into track infos. Copies every
    /// string; no ownership is taken of any mpv memory.
    pub unsafe fn parse_track_list(node: &ffi::mpv_node) -> Vec<TrackInfo> {
        let mut out = Vec::new();
        if node.format != ffi::MPV_FORMAT_NODE_ARRAY {
            return out;
        }
        let arr = node.u.list;
        if arr.is_null() {
            return out;
        }
        let arr = &*arr;
        for i in 0..arr.num.max(0) as usize {
            let item = &*arr.values.add(i);
            if item.format != ffi::MPV_FORMAT_NODE_MAP {
                continue;
            }
            let map = item.u.list;
            if map.is_null() {
                continue;
            }
            let map = &*map;

            let mut id = 0i64;
            let mut kind = None;
            let mut selected = false;
            let mut title: Option<String> = None;
            let mut lang: Option<String> = None;

            for j in 0..map.num.max(0) as usize {
                let key_ptr = *map.keys.add(j);
                if key_ptr.is_null() {
                    continue;
                }
                let key = CStr::from_ptr(key_ptr).to_string_lossy().into_owned();
                let val = &*map.values.add(j);
                match (key.as_str(), val.format) {
                    ("id", ffi::MPV_FORMAT_INT64) => id = unsafe { val.u.int64 },
                    ("type", ffi::MPV_FORMAT_STRING) if !unsafe { val.u.string }.is_null() => {
                        let t = CStr::from_ptr(unsafe { val.u.string })
                            .to_string_lossy()
                            .into_owned();
                        kind = TrackKind::from_type_str(&t);
                    }
                    ("selected", ffi::MPV_FORMAT_FLAG) => selected = unsafe { val.u.flag } != 0,
                    ("title", ffi::MPV_FORMAT_STRING) if !unsafe { val.u.string }.is_null() => {
                        title = Some(
                            CStr::from_ptr(unsafe { val.u.string })
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                    ("lang", ffi::MPV_FORMAT_STRING) if !unsafe { val.u.string }.is_null() => {
                        lang = Some(
                            CStr::from_ptr(unsafe { val.u.string })
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                    _ => {}
                }
            }

            if let Some(kind) = kind {
                let label = title.or(lang).unwrap_or_else(|| format!("轨道 {id}"));
                out.push(TrackInfo {
                    id,
                    kind,
                    selected,
                    label,
                });
            }
        }
        out
    }
}
