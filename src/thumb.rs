//! Progress-bar hover thumbnails.
//!
//! A single owned worker thread performs every libmpv operation and all raw
//! thumbnail file and pixel-buffer work. UI callers only consult shared cache
//! state and publish commands. The worker keeps at most the newest request,
//! cancels obsolete work, and dispatches completed images back through Slint's
//! event loop.

use crate::ffi;
use crate::player::err_str;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const CACHE_CAP: usize = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const FAILURE_BACKOFF: Duration = Duration::from_secs(2);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BOX_W: u32 = 256;
const BOX_H: u32 = 144;

type PixelBuffer = slint::SharedPixelBuffer<slint::Rgb8Pixel>;
type ResultHandler = Box<dyn Fn(&str, slint::Image) + Send>;

struct Shared {
    cache: HashMap<String, PixelBuffer>,
    order: VecDeque<String>,
    failures: HashMap<String, Instant>,
    desired: Option<String>,
    on_result: Option<ResultHandler>,
}

#[derive(Clone, Debug)]
struct Request {
    key: String,
    path: PathBuf,
    target: f64,
    w: u32,
    h: u32,
}

enum Command {
    Request(Request),
    Cancel,
    Shutdown,
}

pub struct Thumbnailer {
    shared: Arc<Mutex<Shared>>,
    sender: Mutex<Option<Sender<Command>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Thumbnailer {
    pub fn new(outdir: PathBuf) -> Result<Thumbnailer, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let shared = Arc::new(Mutex::new(Shared {
            cache: HashMap::new(),
            order: VecDeque::new(),
            failures: HashMap::new(),
            desired: None,
            on_result: None,
        }));
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("thumbnail-worker".into())
            .spawn(move || worker_main(outdir, worker_shared, rx, ready_tx))
            .map_err(|e| format!("thumbnail worker: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Thumbnailer {
                shared,
                sender: Mutex::new(Some(tx)),
                worker: Mutex::new(Some(worker)),
            }),
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                let _ = worker.join();
                Err("thumbnail worker exited during startup".into())
            }
        }
    }

    /// Register the callback for finished thumbnails. It is always invoked by
    /// the Slint event loop, preserving the existing UI-thread expectation.
    pub fn set_result_handler(&self, f: impl Fn(&str, slint::Image) + Send + 'static) {
        self.shared.lock().unwrap().on_result = Some(Box::new(f));
    }

    fn bucket_of(duration: f64) -> f64 {
        (duration / 180.0).floor().clamp(10.0, 60.0)
    }

    /// Look up (or enqueue) a thumbnail for `path` around `time`.
    /// An empty returned image is a recent cached failure and is retried after
    /// a short backoff.
    pub fn request(
        &self,
        path: &Path,
        time: f64,
        duration: f64,
        video_w: i64,
        video_h: i64,
    ) -> (String, Option<slint::Image>) {
        let bucket = Self::bucket_of(duration);
        let target = ((time / bucket).floor() * bucket + bucket / 2.0)
            .clamp(0.0, (duration - 0.05).max(0.0));
        let key = format!("{}|{}", normalize(path), target as u64);
        let (w, h) = thumb_size(video_w, video_h);

        {
            let mut shared = self.shared.lock().unwrap();
            if let Some(pixels) = shared.cache.get(&key).cloned() {
                shared.desired = None;
                return (key, Some(slint::Image::from_rgb8(pixels)));
            }
            if let Some(failed_at) = shared.failures.get(&key).copied() {
                if failed_at.elapsed() < FAILURE_BACKOFF {
                    shared.desired = None;
                    return (key, Some(slint::Image::default()));
                }
                shared.failures.remove(&key);
            }
            if shared.desired.as_deref() == Some(&key) {
                return (key, None);
            }
            shared.desired = Some(key.clone());
        }

        let request = Request {
            key: key.clone(),
            path: path.to_path_buf(),
            target,
            w,
            h,
        };
        let sent = self
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(Command::Request(request)).is_ok());
        if sent {
            (key, None)
        } else {
            let mut shared = self.shared.lock().unwrap();
            if shared.desired.as_deref() == Some(&key) {
                shared.desired = None;
            }
            (key, Some(slint::Image::default()))
        }
    }

    /// Cancel both pending and active hover work. The worker observes this in
    /// at most EVENT_POLL_INTERVAL while libmpv is active.
    pub fn cancel_queued(&self) {
        self.shared.lock().unwrap().desired = None;
        if let Some(tx) = self.sender.lock().unwrap().as_ref() {
            let _ = tx.send(Command::Cancel);
        }
    }

    /// Stop the worker, destroy its current libmpv handle there, clean its
    /// output directory there, and join it. Safe and idempotent.
    pub fn shutdown(&self) {
        self.shared.lock().unwrap().on_result = None;
        if let Some(tx) = self.sender.lock().unwrap().take() {
            let _ = tx.send(Command::Shutdown);
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            if worker.join().is_err() {
                eprintln!("[thumb] worker panicked during shutdown");
            }
        }
    }
}

impl Drop for Thumbnailer {
    fn drop(&mut self) {
        if let Ok(sender) = self.sender.get_mut() {
            if let Some(tx) = sender.take() {
                let _ = tx.send(Command::Shutdown);
            }
        }
        if let Ok(worker) = self.worker.get_mut() {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
        }
    }
}

enum WorkResult {
    Finished(Option<PixelBuffer>),
    Superseded(Option<Request>),
    Shutdown,
}

fn worker_main(
    outdir: PathBuf,
    shared: Arc<Mutex<Shared>>,
    rx: Receiver<Command>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        let _ = ready.send(Err(format!("outdir: {e}")));
        return;
    }
    if ready.send(Ok(())).is_err() {
        let _ = std::fs::remove_dir_all(&outdir);
        return;
    }

    let mut next = None;
    'worker: loop {
        let request = match next.take() {
            Some(request) => request,
            None => match receive_latest(&rx) {
                Some(Command::Request(request)) => request,
                Some(Command::Cancel) => continue,
                Some(Command::Shutdown) | None => break,
            },
        };

        match run_request(&outdir, &rx, &request) {
            WorkResult::Finished(image) => {
                complete_request(&shared, request.key, image);
                match drain_latest(&rx) {
                    Some(Command::Request(request)) => next = Some(request),
                    Some(Command::Shutdown) => break 'worker,
                    Some(Command::Cancel) | None => {}
                }
            }
            WorkResult::Superseded(replacement) => next = replacement,
            WorkResult::Shutdown => break,
        }
    }

    let _ = std::fs::remove_dir_all(&outdir);
    eprintln!("[thumb] worker shut down, temp dir cleaned");
}

fn receive_latest(rx: &Receiver<Command>) -> Option<Command> {
    let first = rx.recv().ok()?;
    Some(coalesce(first, rx))
}

fn drain_latest(rx: &Receiver<Command>) -> Option<Command> {
    let first = match rx.try_recv() {
        Ok(command) => command,
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
    };
    Some(coalesce(first, rx))
}

fn coalesce(mut latest: Command, rx: &Receiver<Command>) -> Command {
    while let Ok(command) = rx.try_recv() {
        latest = command;
        if matches!(latest, Command::Shutdown) {
            break;
        }
    }
    latest
}

fn run_request(outdir: &Path, rx: &Receiver<Command>, request: &Request) -> WorkResult {
    if let Some(command) = drain_latest(rx) {
        return command_as_work_result(command);
    }

    let started = SystemTime::now();
    let handle = match create_instance(outdir, request) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("[thumb] instance failed: {e}");
            return WorkResult::Finished(None);
        }
    };
    let deadline = Instant::now() + REQUEST_TIMEOUT;

    loop {
        if let Some(command) = drain_latest(rx) {
            destroy_handle(handle);
            let _ = std::fs::remove_file(outdir.join("out.raw"));
            return command_as_work_result(command);
        }
        if Instant::now() >= deadline {
            eprintln!("[thumb] request timed out: {}", request.key);
            destroy_handle(handle);
            let _ = std::fs::remove_file(outdir.join("out.raw"));
            return WorkResult::Finished(None);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = EVENT_POLL_INTERVAL.min(remaining).as_secs_f64();
        let event = unsafe { ffi::mpv_wait_event(handle, wait) };
        if event.is_null() {
            continue;
        }
        let event = unsafe { &*event };
        match event.event_id {
            ffi::MPV_EVENT_NONE => {}
            ffi::MPV_EVENT_SHUTDOWN => {
                destroy_handle(handle);
                return WorkResult::Finished(None);
            }
            ffi::MPV_EVENT_END_FILE => {
                let (reason, error) = if event.data.is_null() {
                    (ffi::MPV_END_FILE_REASON_EOF, 0)
                } else {
                    let end = unsafe { &*(event.data as *const ffi::mpv_event_end_file) };
                    (end.reason, end.error)
                };
                destroy_handle(handle);
                if reason != ffi::MPV_END_FILE_REASON_EOF {
                    let detail = if error == 0 {
                        String::new()
                    } else {
                        format!(" ({})", err_str(error))
                    };
                    eprintln!("[thumb] load ended with reason {reason}{detail}");
                    return WorkResult::Finished(None);
                }
                let stale_guard = started - Duration::from_millis(200);
                return WorkResult::Finished(read_raw_output(
                    outdir,
                    stale_guard,
                    request.w,
                    request.h,
                ));
            }
            ffi::MPV_EVENT_LOG_MESSAGE if !event.data.is_null() => unsafe {
                let msg = &*(event.data as *const ffi::mpv_event_log_message);
                if !msg.text.is_null() {
                    let text = CStr::from_ptr(msg.text).to_string_lossy();
                    let text = text.trim_end();
                    if !text.is_empty() {
                        eprintln!("[thumb-mpv] {text}");
                    }
                }
            },
            _ => {}
        }
    }
}

fn command_as_work_result(command: Command) -> WorkResult {
    match command {
        Command::Request(request) => WorkResult::Superseded(Some(request)),
        Command::Cancel => WorkResult::Superseded(None),
        Command::Shutdown => WorkResult::Shutdown,
    }
}

fn complete_request(shared: &Arc<Mutex<Shared>>, key: String, pixels: Option<PixelBuffer>) {
    {
        let mut state = shared.lock().unwrap();
        if let Some(pixels) = &pixels {
            state.failures.remove(&key);
            insert_cache(&mut state, key.clone(), pixels.clone());
        } else {
            state.failures.insert(key.clone(), Instant::now());
        }
        if state.desired.as_deref() == Some(&key) {
            state.desired = None;
        }
    }

    let callback_state = shared.clone();
    if slint::invoke_from_event_loop(move || {
        let image = pixels.map(slint::Image::from_rgb8).unwrap_or_default();
        let state = callback_state.lock().unwrap();
        if let Some(callback) = &state.on_result {
            callback(&key, image);
        }
    })
    .is_err()
    {
        eprintln!("[thumb] result dropped because the UI event loop is unavailable");
    }
}

/// The raw pointer is created, used, and destroyed only by the worker thread.
fn create_instance(outdir: &Path, request: &Request) -> Result<*mut ffi::mpv_handle, String> {
    unsafe {
        let handle = ffi::mpv_create();
        if handle.is_null() {
            return Err("mpv_create() failed".into());
        }
        let out_path = outdir.join("out.raw");
        let _ = std::fs::remove_file(&out_path);
        let out = out_path.to_string_lossy().replace('\\', "/");
        let options = [
            ("config", "no".to_string()),
            ("load-scripts", "no".to_string()),
            ("idle", "yes".to_string()),
            ("o", out),
            ("of", "rawvideo".to_string()),
            ("ovc", "rawvideo".to_string()),
            ("ao", "null".to_string()),
            ("audio", "no".to_string()),
            ("hr-seek", "no".to_string()),
            ("frames", "3".to_string()),
            ("keep-open", "no".to_string()),
            ("pause", "no".to_string()),
            ("hwdec", "no".to_string()),
            (
                "vf",
                format!("scale={}:{},format=rgb24", request.w, request.h),
            ),
        ];
        for (key, value) in &options {
            let key_c = CString::new(*key).unwrap();
            let value_c = CString::new(value.as_str()).unwrap();
            let result = ffi::mpv_set_property_string(handle, key_c.as_ptr(), value_c.as_ptr());
            if result < 0 {
                ffi::mpv_terminate_destroy(handle);
                return Err(format!("option {key}={value} rejected ({result})"));
            }
        }
        let result = ffi::mpv_initialize(handle);
        if result < 0 {
            ffi::mpv_terminate_destroy(handle);
            return Err(format!("mpv_initialize failed ({result})"));
        }
        let level = if std::env::var("NEKO_THUMB_VERBOSE").is_ok() {
            "warn"
        } else {
            "error"
        };
        let level_c = CString::new(level).unwrap();
        ffi::mpv_request_log_messages(handle, level_c.as_ptr());

        let path = request.path.to_string_lossy().replace('\\', "/");
        let path = path.replace('"', "\\\"");
        let command = CString::new(format!(
            "loadfile \"{path}\" replace -1 start={:.2}",
            request.target
        ))
        .unwrap();
        let result = ffi::mpv_command_string(handle, command.as_ptr());
        if result < 0 {
            ffi::mpv_terminate_destroy(handle);
            return Err(format!("loadfile failed ({result})"));
        }
        Ok(handle)
    }
}

fn destroy_handle(handle: *mut ffi::mpv_handle) {
    unsafe { ffi::mpv_terminate_destroy(handle) }
}

fn thumb_size(video_w: i64, video_h: i64) -> (u32, u32) {
    if video_w <= 0 || video_h <= 0 {
        return (BOX_W, BOX_H);
    }
    let scale = (BOX_W as f64 / video_w as f64).min(BOX_H as f64 / video_h as f64);
    let even = |value: f64| -> u32 { (value.round() as u32 / 2) * 2 };
    (
        even(video_w as f64 * scale).max(2),
        even(video_h as f64 * scale).max(2),
    )
}

fn read_raw_output(outdir: &Path, stale_guard: SystemTime, w: u32, h: u32) -> Option<PixelBuffer> {
    let path = outdir.join("out.raw");
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    let expected = (w as usize) * (h as usize) * 3;
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
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(w, h);
    buffer.make_mut_bytes().copy_from_slice(&bytes);
    Some(buffer)
}

fn dump_frame(bytes: &[u8], w: u32, h: u32) {
    let Ok(dir) = std::env::var("NEKO_THUMB_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let max = bytes.iter().copied().max().unwrap_or(0);
    let mean = bytes.iter().map(|byte| *byte as u64).sum::<u64>() as f64 / bytes.len() as f64;
    eprintln!("[thumb] frame stats: max={max} mean={mean:.1}");
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_millis())
        .unwrap_or(0);
    let _ = write_bmp(
        Path::new(&dir).join(format!("thumb-{stamp}-{w}x{h}.bmp")),
        w,
        h,
        bytes,
    );
}

fn write_bmp(path: PathBuf, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let size = 54 + row * h as usize;
    let mut file = Vec::with_capacity(size);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(size as u32).to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&(w as i32).to_le_bytes());
    file.extend_from_slice(&(h as i32).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&24u16.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&((row * h as usize) as u32).to_le_bytes());
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h as usize).rev() {
        let source = y * w as usize * 3;
        for x in 0..w as usize {
            let index = source + x * 3;
            file.push(rgb[index + 2]);
            file.push(rgb[index + 1]);
            file.push(rgb[index]);
        }
        file.resize(file.len() + row - w as usize * 3, 0);
    }
    std::fs::write(path, file)
}

fn insert_cache(shared: &mut Shared, key: String, pixels: PixelBuffer) {
    if shared.cache.insert(key.clone(), pixels).is_none() {
        shared.order.push_back(key);
    }
    while shared.order.len() > CACHE_CAP {
        let evict = shared.order.pop_front().unwrap();
        shared.cache.remove(&evict);
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
        assert_eq!(thumb_size(1080, 1920), (80, 144));
        assert_eq!(thumb_size(0, 0), (256, 144));
        assert_eq!(thumb_size(1440, 1080), (192, 144));
    }

    #[test]
    fn commands_coalesce_to_latest_request() {
        let (tx, rx) = mpsc::channel();
        let request = |key: &str| Request {
            key: key.into(),
            path: key.into(),
            target: 0.0,
            w: 2,
            h: 2,
        };
        tx.send(Command::Request(request("first"))).unwrap();
        tx.send(Command::Cancel).unwrap();
        tx.send(Command::Request(request("latest"))).unwrap();
        match receive_latest(&rx) {
            Some(Command::Request(request)) => assert_eq!(request.key, "latest"),
            _ => panic!("latest request was not retained"),
        }
    }

    #[test]
    fn shutdown_wins_during_coalescing() {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Cancel).unwrap();
        tx.send(Command::Shutdown).unwrap();
        assert!(matches!(receive_latest(&rx), Some(Command::Shutdown)));
    }

    #[test]
    #[ignore = "requires NEKO_TEST_MEDIA pointing to a real video"]
    fn generates_thumbnail_per_request() {
        let media = std::env::var("NEKO_TEST_MEDIA")
            .expect("set NEKO_TEST_MEDIA to run this integration test");
        let outdir = std::env::temp_dir().join("neko-player-thumb-test");
        let _ = std::fs::remove_dir_all(&outdir);
        let thumb = Thumbnailer::new(outdir).expect("thumbnailer init");

        for time in [5.0, 60.0, 200.0] {
            let (key, cached) = thumb.request(Path::new(&media), time, 600.0, 2304, 1440);
            assert!(cached.is_none(), "first request must be pending");
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            let mut image = slint::Image::default();
            while Instant::now() < deadline {
                if let Some(done) = thumb.shared.lock().unwrap().cache.get(&key).cloned() {
                    image = slint::Image::from_rgb8(done);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let size = image.size();
            assert!(size.width > 0 && size.height > 0, "empty thumbnail");
        }
        let (_, cached) = thumb.request(Path::new(&media), 4.0, 600.0, 2304, 1440);
        assert!(cached.is_some(), "expected cached thumbnail");
        thumb.shutdown();
    }
}
