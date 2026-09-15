//! Restore and focus a window that received an external open request
//! (double-click, taskbar jump list) while it was minimized.
//!
//! On Windows, opening a file while the window is minimized only queues the
//! file in the running instance; nothing brings the window back, so the user
//! has to restore it by hand. This module provides a helper that inspects the
//! native window state and, only when the window is actually minimized,
//! restores and focuses it. A window that is merely in the background is left
//! alone, so opening a file never steals focus from the active application.

use gpui_kit::Window;

/// Restore and focus `window` if it is currently minimized.
///
/// On Windows this checks the native window handle and calls
/// [`Window::activate_window`], which restores a minimized window and brings
/// it to the foreground. On other platforms, and for windows that are merely
/// inactive (not minimized), this is a no-op.
pub fn restore_minimized_window(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        if is_minimized(window) {
            log::info!("Window is minimized: restoring and focusing it");
            window.activate_window();
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
    }
}

/// Returns whether the native window behind `window` is minimized (iconified).
#[cfg(target_os = "windows")]
fn is_minimized(window: &Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::IsIconic,
    };

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => {
            let hwnd = HWND(win32.hwnd.get() as *mut _);
            unsafe { IsIconic(hwnd).as_bool() }
        }
        _ => false,
    }
}