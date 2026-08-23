//! Win32 integration: borderless-but-resizable window chrome.
//!
//! We remove `WS_CAPTION` (the system title bar) while keeping
//! `WS_THICKFRAME`, the same technique Chrome / VS Code use. The window
//! keeps native edge-resize and Aero snap, and the title bar is drawn by
//! the Slint UI. Dragging uses the native move loop via
//! `WM_NCLBUTTONDOWN / HTCAPTION`, so it feels exactly like a real
//! title-bar drag (including snap preview).

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::os::raw::{c_int, c_uint};

pub type HWND = *mut c_void;

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
}

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
    fn DragQueryFileW(hDrop: *mut c_void, iFile: c_uint, lpszFile: *mut u16, cch: c_uint) -> c_uint;
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

/// Accept OS file drops anywhere on the window and forward the paths to
/// `on_files` (runs on the UI thread, inside the window message pump).
/// The callback is intentionally leaked with the subclass; it lives as long
/// as the single application window.
pub fn install_file_drop(hwnd: HWND, on_files: DropCallback) {
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
        } else {
            eprintln!("[neko] file drop installed (hwnd={:p})", hwnd);
        }
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
