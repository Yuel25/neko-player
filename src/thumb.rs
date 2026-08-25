//! Progress-bar hover thumbnails.
//!
//! Each request spins up a throwaway libmpv instance in encode mode
//! (`o=<file> --of=rawvideo --ovc=rawvideo`) that decodes a frame near the
//! requested position into a raw RGB24 file. A fresh instance per request is
//! required: the encode VO can only run one session per handle, and
//! destroying the handle also flushes the encoder synchronously, so the
//! output bytes are complete as soon as the handle is gone. (This build's
//! libmpv has no `vo=image`; the encoder route is the same mechanism the
//! thumbfast script uses with the mpv binary.) Results are cached per
//! file+time-bucket (LRU) and delivered through `set_result_handler` on the
//! UI thread.

use crate::ffi;
use crate::player::err_str;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Cached thumbnails per file (LRU beyond this).
const CACHE_CAP: usize = 100;
/// A pending request older than this is considered lost; the bucket is
/// marked failed and the next request may replace it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
/// Preview box the thumbnails are scaled into (px); aspect is preserved with
/// even dimensions as required by the rawvideo encoder.
const BOX_W: u32 = 256;
const BOX_H: u32 = 144;

struct Pending {
    key: String,
    w: u32,
    h: u32,
    started: SystemTime,
    requested_at: Instant,
}
struct Queued {
    key: String,
    path: PathBuf,
    target: f64,
    w: u32,
    h: u32,
}

type ResultHandler = Box<dyn Fn(&str, slint::Image) + Send>;

pub struct Inner {
    cache: HashMap<String, slint::Image>,
    order: VecDeque<String>,
    /// Handle of the current per-request instance (null when idle).
    handle: *mut ffi::mpv_handle,
    wakeup_ctx: *mut c_void,
    pending: Option<Pending>,
    queued: Option<Queued>,
    /// Delivered on the UI thread whenever a request finishes.
    on_result: Option<ResultHandler>,
}

pub struct Thumbnailer {
    inner: Arc<Mutex<Inner>>,
    outdir: PathBuf,
}

// The current handle and poll/finish only ever run on the UI thread; the mpv
// wakeup callback itself never calls back into mpv.
unsafe impl Send for Thumbnailer {}
unsafe impl Sync for Thumbnailer {}
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Thumbnailer {
    pub fn new(outdir: PathBuf) -> Result<Thumbnailer, String> {
        std::fs::create_dir_all(&outdir).map_err(|e| format!("outdir: {e}"))?;
        Ok(Thumbnailer {
            inner: Arc::new(Mutex::new(Inner {
                cache: HashMap::new(),
                order: VecDeque::new(),
                handle: std::ptr::null_mut(),
                wakeup_ctx: std::ptr::null_mut(),
                pending: None,
                queued: None,
                on_result: None,
            })),
            outdir,
        })
    }

    /// Register the UI-thread callback for finished thumbnails.
    pub fn set_result_handler(&self, f: impl Fn(&str, slint::Image) + Send + 'static) {
        self.inner.lock().unwrap().on_result = Some(Box::new(f));
    }

    /// Bucket width for cache keys: grows with duration so a sweep across
    /// the bar does not fire hundreds of loads, but stays fine enough to
    /// match what is under the cursor.
    fn bucket_of(duration: f64) -> f64 {
        (duration / 180.0).floor().clamp(10.0, 60.0)
    }

    /// Look up (or start) a thumbnail for `path` around `time`.
    /// Returns the cache key and the image if it was already available
    /// (an empty image is a cached failure).
    pub fn request(
        &self,
        path: &Path,
        time: f64,
        duration: f64,
        video_w: i64,
        video_h: i64,
    ) -> (String, Option<slint::Image>) {
        let bucket = Self::bucket_of(duration);
        // Seek to the middle of the bucket so any cursor position in it maps
        // to the same thumbnail.
        let target = ((time / bucket).floor() * bucket + bucket / 2.0)
            .clamp(0.0, (duration - 0.05).max(0.0));
        let key = format!("{}|{}", normalize(path), target as u64);
        let (w, h) = thumb_size(video_w, video_h);

        let mut inner = self.inner.lock().unwrap();
        if let Some(img) = inner.cache.get(&key).cloned() {
            inner.queued = None;
            return (key, Some(img));
        }
        if let Some(p) = &inner.pending {
            if p.key == key && p.requested_at.elapsed() < REQUEST_TIMEOUT {
                inner.queued = None;
                return (key, None); // already being generated
            }
            if p.requested_at.elapsed() >= REQUEST_TIMEOUT {
                eprintln!("[thumb] request timed out; restarting");
                destroy_current(&mut inner);
                inner.pending = None;
            } else {
                inner.queued = Some(Queued {
                    key: key.clone(),
                    path: path.to_path_buf(),
                    target,
                    w,
                    h,
                });
                return (key, None);
            }
        }
        inner.pending = Some(Pending {
            key: key.clone(),
            w,
            h,
            started: SystemTime::now(),
            requested_at: Instant::now(),
        });
        drop(inner);

        match spawn_instance(&self.outdir, &self.inner, path, target, w, h) {
            Ok(()) => (key, None),
            Err(e) => {
                eprintln!("[thumb] instance failed: {e}");
                let mut inner = self.inner.lock().unwrap();
                inner.pending = None;
                (key, Some(slint::Image::default()))
            }
        }
    }

    /// Process pending mpv events of the current instance; the app relies on
    /// the wakeup bounce in `spawn_instance`, tests poll this directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn poll(&self) {
        process_events(&self.inner, &self.outdir);
    }

    pub fn cancel_queued(&self) {
        self.inner.lock().unwrap().queued = None;
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        destroy_current(&mut inner);
        inner.queued = None;
        inner.on_result = None;
        drop(inner);
        let _ = std::fs::remove_dir_all(&self.outdir);
        eprintln!("[thumb] shut down, temp dir cleaned");
    }
}

/// The one event pump: drains the current instance and finalizes the request
/// on END_FILE (destroying the handle flushes the encoder synchronously).
fn process_events(inner: &Arc<Mutex<Inner>>, outdir: &Path) {
    let handle = {
        let guard = inner.lock().unwrap();
        if guard.handle.is_null() {
            return;
        }
        guard.handle
    };
    unsafe {
        loop {
            let ev_ptr = ffi::mpv_wait_event(handle, 0.0);
            if ev_ptr.is_null() {
                break;
            }
            let ev = &*ev_ptr;
            match ev.event_id {
                ffi::MPV_EVENT_NONE | ffi::MPV_EVENT_SHUTDOWN => break,
                ffi::MPV_EVENT_END_FILE => {
                    let mut reason = crate::ffi::MPV_END_FILE_REASON_EOF;
                    let mut error = 0;
                    if !ev.data.is_null() {
                        let end = &*(ev.data as *const ffi::mpv_event_end_file);
                        reason = end.reason;
                        error = end.error;
                    }
                    if reason == ffi::MPV_END_FILE_REASON_EOF {
                        finish(inner, outdir, None);
                    } else {
                        let detail = if error == 0 {
                            String::new()
                        } else {
                            format!(" ({})", err_str(error))
                        };
                        eprintln!("[thumb] load ended with reason {reason}{detail}");
                        finish(inner, outdir, Some(slint::Image::default()));
                    }
                    break; // the request is gone either way
                }
                ffi::MPV_EVENT_LOG_MESSAGE if !ev.data.is_null() => {
                    let msg = &*(ev.data as *const ffi::mpv_event_log_message);
                    if !msg.text.is_null() {
                        let text = CStr::from_ptr(msg.text).to_string_lossy();
                        let text = text.trim_end();
                        if !text.is_empty() {
                            eprintln!("[thumb-mpv] {text}");
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Completion path: destroy (flushes the encoder), read the bytes unless
/// `forced` marks the request failed, cache and notify.
fn finish(inner: &Arc<Mutex<Inner>>, outdir: &Path, forced: Option<slint::Image>) {
    let pending = {
        let mut guard = inner.lock().unwrap();
        destroy_current(&mut guard);
        guard.pending.take()
    };
    let Some(p) = pending else { return };
    let img = forced
        .or_else(|| {
            let stale_guard = p.started - Duration::from_millis(200);
            read_raw_output(outdir, stale_guard, p.w, p.h)
        })
        .unwrap_or_default();
    let queued = {
        let mut guard = inner.lock().unwrap();
        if img.size().width > 0 && img.size().height > 0 {
            insert_cache(&mut guard, p.key.clone(), img.clone());
        }
        if let Some(cb) = &guard.on_result {
            cb(&p.key, img);
        }
        guard.queued.take()
    };
    if let Some(q) = queued {
        start_queued(inner, outdir, q);
    }
}

fn start_queued(inner: &Arc<Mutex<Inner>>, outdir: &Path, q: Queued) {
    {
        let mut guard = inner.lock().unwrap();
        if let Some(img) = guard.cache.get(&q.key) {
            let img = img.clone();
            if let Some(cb) = &guard.on_result {
                cb(&q.key, img);
            }
            return;
        }
        guard.pending = Some(Pending {
            key: q.key,
            w: q.w,
            h: q.h,
            started: SystemTime::now(),
            requested_at: Instant::now(),
        });
    }
    match spawn_instance(outdir, inner, &q.path, q.target, q.w, q.h) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[thumb] queued instance failed: {e}");
            inner.lock().unwrap().pending = None;
        }
    }
}

/// Tear down the current per-request instance (synchronous encoder flush).
fn destroy_current(inner: &mut Inner) {
    if !inner.handle.is_null() {
        unsafe {
            ffi::mpv_set_wakeup_callback(inner.handle, None, std::ptr::null_mut());
            ffi::mpv_terminate_destroy(inner.handle);
        }
        inner.handle = std::ptr::null_mut();
    }
    if !inner.wakeup_ctx.is_null() {
        unsafe {
            drop(Box::from_raw(inner.wakeup_ctx as *mut Box<dyn Fn() + Send>));
        }
        inner.wakeup_ctx = std::ptr::null_mut();
    }
}

/// Create and arm a fresh encode-mode instance for one request.
fn spawn_instance(
    outdir: &Path,
    inner: &Arc<Mutex<Inner>>,
    path: &Path,
    target: f64,
    w: u32,
    h: u32,
) -> Result<(), String> {
    unsafe {
        let handle = ffi::mpv_create();
        if handle.is_null() {
            return Err("mpv_create() failed".into());
        }
        let out = outdir.join("out.raw").to_string_lossy().replace('\\', "/");
        let options = [
            ("config", "no".to_string()),
            ("load-scripts", "no".to_string()),
            ("idle", "yes".to_string()),
            // Encode mode: raw RGB24 bytes; a couple of frames are decoded
            // so the lavc VO actually encodes one before playback ends.
            ("o", out),
            ("of", "rawvideo".to_string()),
            ("ovc", "rawvideo".to_string()),
            ("ao", "null".to_string()),
            ("audio", "no".to_string()),
            // Keyframe seeks only: fast enough to feel instant on hover.
            ("hr-seek", "no".to_string()),
            ("frames", "3".to_string()),
            ("keep-open", "no".to_string()),
            ("pause", "no".to_string()),
            ("hwdec", "no".to_string()),
            ("vf", format!("scale={w}:{h},format=rgb24")),
        ];
        for (k, v) in &options {
            let ck = CString::new(*k).unwrap();
            let cv = CString::new(v.as_str()).unwrap();
            let r = ffi::mpv_set_property_string(handle, ck.as_ptr(), cv.as_ptr());
            if r < 0 {
                let msg = format!("option {k}={v} rejected ({r})");
                ffi::mpv_terminate_destroy(handle);
                return Err(msg);
            }
        }
        let r = ffi::mpv_initialize(handle);
        if r < 0 {
            ffi::mpv_terminate_destroy(handle);
            return Err(format!("mpv_initialize failed ({r})"));
        }
        let level = if std::env::var("NEKO_THUMB_VERBOSE").is_ok() {
            "warn"
        } else {
            "error"
        };
        let c = CString::new(level).unwrap();
        ffi::mpv_request_log_messages(handle, c.as_ptr());

        // Wakeup: bounce into the Slint loop, which polls the current
        // instance. The context Box is intentionally leaked for the process
        // lifetime; the callback never touches mpv itself.
        let signal: Box<dyn Fn() + Send> = {
            let inner = inner.clone();
            let outdir = outdir.to_path_buf();
            Box::new(move || {
                let inner = inner.clone();
                let outdir = outdir.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    process_events(&inner, &outdir);
                });
            })
        };
        let ctx = Box::into_raw(Box::new(signal)) as *mut c_void;
        unsafe extern "C" fn trampoline(ctx: *mut c_void) {
            let f = &*(ctx as *const Box<dyn Fn() + Send>);
            f();
        }
        ffi::mpv_set_wakeup_callback(handle, Some(trampoline), ctx);
        // Publish before loadfile can trigger a wakeup.
        {
            let mut guard = inner.lock().unwrap();
            guard.handle = handle;
            guard.wakeup_ctx = ctx;
        }

        // `start` is consumed by mpv after one file, so it rides the loadfile
        // command as a per-load option.
        let p = path.to_string_lossy().replace('\\', "/");
        let p = p.replace('"', "\\\"");
        let c = CString::new(format!("loadfile \"{p}\" replace -1 start={target:.2}")).unwrap();
        let r = ffi::mpv_command_string(handle, c.as_ptr());
        if r < 0 {
            destroy_current(&mut inner.lock().unwrap());
            return Err(format!("loadfile failed ({r})"));
        }
        Ok(())
    }
}

/// Even-sized box fit inside BOX_W x BOX_H.
fn thumb_size(video_w: i64, video_h: i64) -> (u32, u32) {
    if video_w <= 0 || video_h <= 0 {
        return (BOX_W, BOX_H);
    }
    let scale = (BOX_W as f64 / video_w as f64).min(BOX_H as f64 / video_h as f64);
    let even = |v: f64| -> u32 { (v.round() as u32 / 2) * 2 };
    (
        even(video_w as f64 * scale).max(2),
        even(video_h as f64 * scale).max(2),
    )
}

/// Read the fixed raw RGB24 output if it is complete AND was written after
/// `stale_guard` (so leftovers of a superseded request never pass).
fn read_raw_output(outdir: &Path, stale_guard: SystemTime, w: u32, h: u32) -> Option<slint::Image> {
    let path = outdir.join("out.raw");
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    let expected = (w as usize) * (h as usize) * 3;
    // `frames=3` slack means the muxer may hold extra frames; the first
    // frame's bytes are what we want.
    if modified < stale_guard || (meta.len() as usize) < expected {
        return None;
    }
    let mut bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    if bytes.len() < expected {
        return None;
    }
    bytes.truncate(expected);
    dump_frame(&bytes, w, h);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(&bytes);
    Some(slint::Image::from_rgb8(buf))
}

/// NEKO_THUMB_DUMP=<dir> writes each generated frame as a BMP for
/// debugging what the encoder actually produced.
fn dump_frame(bytes: &[u8], w: u32, h: u32) {
    let Ok(dir) = std::env::var("NEKO_THUMB_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let max = bytes.iter().copied().max().unwrap_or(0);
    let mean = bytes.iter().map(|b| *b as u64).sum::<u64>() as f64 / bytes.len() as f64;
    eprintln!("[thumb] frame stats: max={max} mean={mean:.1}");
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    let _ = write_bmp(
        Path::new(&dir).join(format!("thumb-{stamp}-{w}x{h}.bmp")),
        w,
        h,
        bytes,
    );
}

/// Minimal 24-bit BMP writer (debug dumps only).
fn write_bmp(path: PathBuf, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let size = 54 + row * h as usize;
    let mut f = Vec::with_capacity(size);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&(size as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(h as i32).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&((row * h as usize) as u32).to_le_bytes());
    f.extend_from_slice(&2835u32.to_le_bytes());
    f.extend_from_slice(&2835u32.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h as usize).rev() {
        let src = y * w as usize * 3;
        for x in 0..w as usize {
            let i = src + x * 3;
            f.push(rgb[i + 2]);
            f.push(rgb[i + 1]);
            f.push(rgb[i]);
        }
        f.resize(f.len() + row - w as usize * 3, 0);
    }
    std::fs::write(path, f)
}

fn insert_cache(inner: &mut Inner, key: String, img: slint::Image) {
    if inner.cache.insert(key.clone(), img).is_none() {
        inner.order.push_back(key);
    }
    while inner.order.len() > CACHE_CAP {
        let evict = inner.order.pop_front().unwrap();
        inner.cache.remove(&evict);
    }
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_size_fits_box_with_even_sides() {
        assert_eq!(thumb_size(1920, 1080), (256, 144));
        assert_eq!(thumb_size(1080, 1920), (80, 144)); // 80.875 -> 80 (even)
        assert_eq!(thumb_size(0, 0), (256, 144));
        assert_eq!(thumb_size(1440, 1080), (192, 144)); // 4:3
    }

    /// End-to-end probe of the per-request encoder pipeline against a real
    /// file. Set NEKO_TEST_MEDIA to a video path to run:
    /// `NEKO_TEST_MEDIA=x.mp4 cargo test generates_thumbnail -- --nocapture`
    #[test]
    #[ignore = "requires NEKO_TEST_MEDIA pointing to a real video"]
    fn generates_thumbnail_per_request() {
        let media = std::env::var("NEKO_TEST_MEDIA")
            .expect("set NEKO_TEST_MEDIA to run this integration test");
        let outdir = std::env::temp_dir().join("neko-player-thumb-test");
        let _ = std::fs::remove_dir_all(&outdir);
        let thumb = Thumbnailer::new(outdir).expect("thumbnailer init");

        // Multiple sample points with the file's real dimensions; each must
        // run its own instance end to end. (Without a Slint event loop the
        // wakeup bounce goes nowhere, so tests poll directly.)
        for time in [5.0, 60.0, 200.0] {
            let (key, cached) = thumb.request(Path::new(&media), time, 600.0, 2304, 1440);
            assert!(cached.is_none(), "first request must be pending");

            let deadline = Instant::now() + Duration::from_secs(25);
            let mut img = slint::Image::default();
            while Instant::now() < deadline {
                thumb.poll();
                let got = thumb.inner.lock().unwrap().cache.get(&key).cloned();
                if let Some(done) = got {
                    img = done;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let size = img.size();
            assert!(
                size.width > 0 && size.height > 0,
                "empty image (encoder pipeline failed?)"
            );
            eprintln!("thumbnail @{time}s {}x{}", size.width, size.height);
        }

        // Same bucket must now be served from the cache.
        let (_, cached) = thumb.request(Path::new(&media), 4.0, 600.0, 2304, 1440);
        assert!(cached.is_some(), "expected cached thumbnail");
        thumb.shutdown();
    }
}
