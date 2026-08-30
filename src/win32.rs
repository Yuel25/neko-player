//! Win32 integration: borderless-but-resizable window chrome.
//!
//! We remove `WS_CAPTION` (the system title bar) while keeping
//! `WS_THICKFRAME`, the same technique Chrome / VS Code use. The window
//! keeps native edge-resize and Aero snap, and the title bar is drawn by
//! the Slint UI. Dragging uses the native move loop via
//! `WM_NCLBUTTONDOWN / HTCAPTION`, so it feels exactly like a real
//! title-bar drag (including snap preview).

#![allow(non_snake_case, non_camel_case_types, dead_code)]
#![allow(clippy::upper_case_acronyms)] // Preserve canonical Win32 ABI names.

use std::ffi::c_void;
use std::os::raw::{c_int, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};

pub type HWND = *mut c_void;
pub type HANDLE = *mut c_void;

const GWL_STYLE: c_int = -16;
const WS_CAPTION: isize = 0x00C0_0000;
const WS_THICKFRAME: isize = 0x0004_0000;

const SWP_NOSIZE: c_uint = 0x0001;
const SWP_NOMOVE: c_uint = 0x0002;
const SWP_NOZORDER: c_uint = 0x0004;
const SWP_NOACTIVATE: c_uint = 0x0010;
const SWP_FRAMECHANGED: c_uint = 0x0020;

const WM_NCLBUTTONDOWN: c_uint = 0x00A1;
const HTCAPTION: isize = 2;
const WM_DROPFILES: c_uint = 0x0233;

/// Invoked on the UI thread with the paths of files dropped onto the window.
pub type DropCallback = Box<dyn Fn(Vec<std::path::PathBuf>)>;

/// RenderingSetup can run more than once per window (GL context recreation);
/// the subclass survives rendering teardowns, so only the first install takes
/// effect. Re-calling SetWindowSubclass with the same id would leak the
/// previous callback Box.
static DROP_SUBCLASS_INSTALLED: AtomicBool = AtomicBool::new(false);
type SubclassProc = unsafe extern "system" fn(
    hWnd: HWND,
    uMsg: c_uint,
    wParam: usize,
    lParam: isize,
    uIdSubclass: usize,
    dwRefData: *mut c_void,
) -> isize;

// DWM window attributes
const DWMWA_USE_IMMERSIVE_DARK_MODE_19: c_uint = 19; // Windows 10 1809
const DWMWA_USE_IMMERSIVE_DARK_MODE_20: c_uint = 20; // Windows 10 1903+
const DWMWA_WINDOW_CORNER_PREFERENCE: c_uint = 33; // Windows 11
const DWMWA_BORDER_COLOR: c_uint = 34; // Windows 11
const DWMWCP_ROUND: c_int = 2;

#[link(name = "user32")]
extern "system" {
    fn GetWindowLongPtrW(hWnd: HWND, nIndex: c_int) -> isize;
    fn SetWindowLongPtrW(hWnd: HWND, nIndex: c_int, dwNewLong: isize) -> isize;
    fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: c_int,
        Y: c_int,
        cx: c_int,
        cy: c_int,
        uFlags: c_uint,
    ) -> c_int;
    fn ReleaseCapture() -> c_int;
    fn SendMessageW(hWnd: HWND, Msg: c_uint, wParam: usize, lParam: isize) -> isize;
    fn GetWindowPlacement(hWnd: HWND, lpwndpl: *mut WINDOWPLACEMENT) -> c_int;
    fn ShowWindow(hWnd: HWND, nCmdShow: c_int) -> c_int;
    fn MonitorFromPoint(pt: POINT, dwFlags: c_uint) -> HWND;
    fn GetMonitorInfoW(hMonitor: HWND, lpmi: *mut MONITORINFO) -> c_int;
    fn MessageBoxW(hWnd: HWND, lpText: *const u16, lpCaption: *const u16, uType: c_uint) -> c_int;
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(dwDesiredAccess: c_uint, bInheritHandle: c_int, dwProcessId: c_uint) -> HANDLE;
    fn CloseHandle(hObject: HANDLE) -> c_int;
    fn GetLastError() -> c_uint;
}

pub fn show_error(message: &str) {
    const MB_OK: c_uint = 0;
    const MB_ICONERROR: c_uint = 0x10;
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "neko player - 图形初始化失败"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct POINT {
    pub x: c_int,
    pub y: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RECT {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

#[repr(C)]
pub struct WINDOWPLACEMENT {
    pub length: c_uint,
    pub flags: c_uint,
    pub showCmd: c_uint,
    pub ptMinPosition: POINT,
    pub ptMaxPosition: POINT,
    pub rcNormalPosition: RECT,
}

#[repr(C)]
pub struct MONITORINFO {
    pub cbSize: c_uint,
    pub rcMonitor: RECT,
    pub rcWork: RECT,
    pub dwFlags: c_uint,
}

const SW_MAXIMIZE: c_int = 3;
const SW_SHOWMAXIMIZED: c_uint = 3;
const MONITOR_DEFAULTTONEAREST: c_uint = 2;
const PROCESS_QUERY_LIMITED_INFORMATION: c_uint = 0x1000;
const ERROR_INVALID_PARAMETER: c_uint = 87;
const WM_NCDESTROY: c_uint = 0x0082;

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        dwAttribute: c_uint,
        pvAttribute: *const c_void,
        cbAttribute: c_uint,
    ) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn DragAcceptFiles(hWnd: HWND, fAccept: c_int);
    fn DragQueryFileW(hDrop: *mut c_void, iFile: c_uint, lpszFile: *mut u16, cch: c_uint)
        -> c_uint;
    fn DragFinish(hDrop: *mut c_void);
}

#[link(name = "comctl32")]
extern "system" {
    fn SetWindowSubclass(
        hWnd: HWND,
        pfnSubclass: SubclassProc,
        uIdSubclass: usize,
        dwRefData: *mut c_void,
    ) -> c_int;
    fn RemoveWindowSubclass(hWnd: HWND, pfnSubclass: SubclassProc, uIdSubclass: usize) -> c_int;
    fn DefSubclassProc(hWnd: HWND, uMsg: c_uint, wParam: usize, lParam: isize) -> isize;
}

#[link(name = "ole32")]
extern "system" {
    fn RevokeDragDrop(hwnd: HWND) -> i32;
}

/// Strip the system caption but keep the thick resize frame, and ask DWM
/// for dark-mode accents plus a subtle border color that matches our theme.
pub fn apply_borderless_chrome(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = (style & !WS_CAPTION) | WS_THICKFRAME;
        if new_style != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        let dark: c_int = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE_20,
            &dark as *const c_int as *const c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE_19,
            &dark as *const c_int as *const c_void,
            4,
        );
        // #2e2e3a as COLORREF (0x00BBGGRR)
        let border: u32 = 0x00_3a_2e_2e;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const c_void,
            4,
        );
        // Windows 11: clip the borderless window with the standard rounded
        // corners (the dark border above follows the rounded outline).
        // Unsupported on Windows 10 — the call fails and is ignored.
        let corners: c_int = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const c_int as *const c_void,
            4,
        );
    }
}

/// Start the native "drag the title bar" move loop.
pub fn begin_system_drag(hwnd: HWND) {
    unsafe {
        ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
    }
}

/// Restorable window rectangle (the normal position even while maximized).
#[derive(Clone, Copy, Debug)]
pub struct SavedRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub maximized: bool,
}

pub fn save_window_rect(hwnd: HWND) -> Option<SavedRect> {
    unsafe {
        let mut pl = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as c_uint,
            flags: 0,
            showCmd: 0,
            ptMinPosition: POINT { x: 0, y: 0 },
            ptMaxPosition: POINT { x: 0, y: 0 },
            rcNormalPosition: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        };
        if GetWindowPlacement(hwnd, &mut pl) == 0 {
            return None;
        }
        let r = pl.rcNormalPosition;
        // rcNormalPosition can be degenerate for never-shown windows.
        if r.right <= r.left || r.bottom <= r.top {
            return None;
        }
        Some(SavedRect {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
            maximized: pl.showCmd == SW_SHOWMAXIMIZED,
        })
    }
}

/// Whether a process with this pid exists (best effort). A pid that cannot
/// even be queried counts as alive so its files are never cleaned up.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            CloseHandle(handle);
            return true;
        }
        GetLastError() != ERROR_INVALID_PARAMETER
    }
}

/// Apply a saved placement, clamped into the nearest monitor's work area so
/// placements saved on a larger or since-unplugged screen stay reachable.
pub fn restore_window_rect(hwnd: HWND, rect: SavedRect) -> bool {
    unsafe {
        let center = POINT {
            x: rect.x + rect.w / 2,
            y: rect.y + rect.h / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            eprintln!("[neko] no monitor available; ignoring saved window position");
            return false;
        }
        let mut x = rect.x;
        let mut y = rect.y;
        let mut w = rect.w;
        let mut h = rect.h;
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as c_uint,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            let work = info.rcWork;
            let work_w = (work.right - work.left).max(1);
            let work_h = (work.bottom - work.top).max(1);
            w = w.clamp(1, work_w);
            h = h.clamp(1, work_h);
            x = x.clamp(work.left, (work.right - w).max(work.left));
            y = y.clamp(work.top, (work.bottom - h).max(work.top));
        }
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        if rect.maximized {
            ShowWindow(hwnd, SW_MAXIMIZE);
        }
        true
    }
}

/// Accept OS file drops anywhere on the window and forward the paths to
/// `on_files` (runs on the UI thread, inside the window message pump).
/// The subclass owns the callback and releases it on WM_NCDESTROY.
pub fn install_file_drop(hwnd: HWND, on_files: DropCallback) {
    if DROP_SUBCLASS_INSTALLED.load(Ordering::Acquire) {
        return;
    }
    unsafe {
        // winit registers its own OLE IDropTarget at window creation, and real
        // Explorer drags are delivered there instead of producing WM_DROPFILES.
        // Revoke it so the system falls back to the DragAcceptFiles path,
        // which our window-proc subclass below receives.
        RevokeDragDrop(hwnd);
        let cb = Box::into_raw(Box::new(on_files));
        DragAcceptFiles(hwnd, 1);
        if SetWindowSubclass(hwnd, drop_proc, 1, cb as *mut c_void) == 0 {
            drop(Box::from_raw(cb));
            DragAcceptFiles(hwnd, 0);
            eprintln!("[neko] SetWindowSubclass failed; drag-and-drop disabled");
            return;
        }
        DROP_SUBCLASS_INSTALLED.store(true, Ordering::Release);
        eprintln!("[neko] file drop installed (hwnd={:p})", hwnd);
    }
}

unsafe extern "system" fn drop_proc(
    hwnd: HWND,
    msg: c_uint,
    wparam: usize,
    lparam: isize,
    _id: usize,
    ref_data: *mut c_void,
) -> isize {
    if msg == WM_NCDESTROY {
        DragAcceptFiles(hwnd, 0);
        RemoveWindowSubclass(hwnd, drop_proc, _id);
        let result = DefSubclassProc(hwnd, msg, wparam, lparam);
        if !ref_data.is_null() {
            drop(Box::from_raw(ref_data as *mut DropCallback));
        }
        return result;
    }
    if msg == WM_DROPFILES {
        let hdrop = wparam as *mut c_void;
        let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
        eprintln!("[neko] WM_DROPFILES: {count} file(s)");
        let mut files = Vec::with_capacity(count as usize);
        for i in 0..count {
            // First query returns the length without the terminating NUL.
            let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
            let mut buf = vec![0u16; len as usize + 1];
            DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as c_uint);
            let path: String = String::from_utf16_lossy(&buf[..buf.len() - 1]);
            files.push(std::path::PathBuf::from(path));
        }
        DragFinish(hdrop);
        if !ref_data.is_null() {
            (*(ref_data as *mut DropCallback))(files);
        }
        return 0;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::process_alive;

    #[test]
    fn detects_own_process_and_rejects_invalid_pid() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
    }
}
