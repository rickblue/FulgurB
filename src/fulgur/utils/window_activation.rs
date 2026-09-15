//! Restore and focus a window that received an external open request
//! (double-click, taskbar jump list) while it was minimized.
//!
//! On Windows, opening a file while the window is minimized only queues the
//! file in the running instance; nothing brings the window back, so the user
//! has to restore it by hand. This module provides two helpers:
//!
//! - [`restore_minimized_window`], used from the render cycle: checks the
//!   window's native state and, only when the window is actually minimized,
//!   restores and focuses it via [`Window::activate_window`]. A window that
//!   is merely in the background is left alone, so opening a file never
//!   steals focus from the active application.
//!
//! - [`restore_minimized_process_windows`], used from the IPC listener
//!   thread: while a window is minimized GPUI pauses its frame loop, so the
//!   render cycle (which drains the pending-files queue) never runs. The
//!   listener therefore restores the minimized window(s) directly with
//!   thread-safe Win32 calls; the resulting `WM_SIZE` resumes the frame loop
//!   so the queued file is opened on the next frame.

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
pub fn is_minimized(window: &Window) -> bool {
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

/// Restore every minimized top-level window of this process and bring the
/// restored windows to the foreground.
///
/// This is called from the IPC listener thread (a background thread) as soon
/// as a file or command has been queued for the render cycle. All Win32
/// calls used here are thread-safe: `ShowWindow` posts a message to the
/// owning window thread, whose message pump keeps running even while GPUI's
/// frame loop is paused, so the frame loop resumes when the window is
/// restored and the queued file is opened on the next frame.
#[cfg(target_os = "windows")]
pub fn restore_minimized_process_windows() {
    use windows::Win32::{
        Foundation::LPARAM,
        UI::WindowsAndMessaging::EnumWindows,
    };

    unsafe {
        let _ = EnumWindows(Some(restore_minimized_windows_callback), LPARAM(0));
    }
}

/// `EnumWindows` callback: restore a top-level window of this process if it
/// is minimized, and bring it to the foreground.
///
/// The simulated Alt key press works around Windows' foreground lock, which
/// would otherwise prevent a background thread from calling
/// `SetForegroundWindow` (the same trick GPUI's own window activation uses).
#[cfg(target_os = "windows")]
unsafe extern "system" fn restore_minimized_windows_callback(
    hwnd: windows::Win32::Foundation::HWND,
    _lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::BOOL {
    use windows::Win32::{
        Foundation::BOOL,
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_MENU,
        },
        UI::WindowsAndMessaging::{
            GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
        },
    };

    let mut pid: u32 = 0;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid != std::process::id() as u32 {
        return BOOL::from(true);
    }
    if !unsafe { IsIconic(hwnd).as_bool() } {
        return BOOL::from(true);
    }

    log::info!("IPC: window is minimized, restoring it");
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        ..Default::default()
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        dwFlags: KEYEVENTF_KEYUP,
                        ..Default::default()
                    },
                },
            },
        ];
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
    }
    BOOL::from(true)
}