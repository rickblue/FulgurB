//! Windows single-instance coordination via TCP loopback.
//!
//! When the taskbar jump list launches a new Fulgur process with a file-path
//! argument, this module detects the already-running instance, forwards the
//! path to it over a loopback TCP connection, and lets the caller exit early.
//!
//! The listening instance receives the path, appends it to the shared
//! `pending_files` queue, and the next render frame opens / focuses the file -
//! the same mechanism used by the macOS "Open With" handler.
//!
//! Jump list Tasks ("New Tab", "New Window") send a `CMD:new-tab` /
//! `CMD:new-window` line instead of a file path. The listener pushes these into
//! `pending_ipc_commands` and the render loop dispatches them in-process.
//!
//! While a window is minimized GPUI pauses its frame loop, so the render
//! cycle would never drain the queues and the window would stay hidden until
//! the user restores it by hand. After queueing a message the listener
//! therefore wakes the main thread ([`wake_minimized_windows`]) to restore
//! any minimized window that is about to process it.

use crate::fulgur::utils::worker::Worker;
use crate::fulgur::utils::window_activation;
use crate::fulgur::window_manager::WindowManager;
use gpui_kit::{App, ForegroundExecutor};
use parking_lot::Mutex;
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

/// Loopback port used for Fulgur IPC. Chosen to be unlikely to conflict.
const IPC_PORT: u16 = 29764;

/// Maximum time a dropped IPC listener worker is joined before being detached.
/// The wakeup self-connect unblocks `accept` immediately, so this is short.
const IPC_WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Prefix used to distinguish command messages from file-path messages.
const CMD_PREFIX: &str = "CMD:";

/// Try to forward `paths` to an already-running Fulgur instance.
///
/// Connects to the loopback listener started by the primary instance.
/// If the connection succeeds the paths are written one per line and the
/// caller should exit immediately. If the connection is refused this is
/// the first instance and the caller should continue normally.
///
/// ### Arguments
/// - `paths`: The file paths to forward to the running instance
///
/// ### Returns
/// - `true`: Another instance was found and the paths were forwarded - caller should exit
/// - `false`: No existing instance is running - caller should continue
#[must_use]
pub fn try_forward_to_existing_instance(paths: &[PathBuf]) -> bool {
    match TcpStream::connect(("127.0.0.1", IPC_PORT)) {
        Ok(mut stream) => {
            for path in paths {
                let _ = writeln!(stream, "{}", path.display());
            }
            log::info!(
                "Single-instance: forwarded {} path(s) to running instance",
                paths.len()
            );
            true
        }
        Err(_) => false,
    }
}

/// Try to send a command to an already-running Fulgur instance.
///
/// Writes a single `CMD:<cmd>` line (e.g. `CMD:new-tab`) to the loopback
/// listener. If the connection succeeds the command has been delivered and
/// the caller should exit immediately. If the connection is refused there is
/// no existing instance and the caller should start normally.
///
/// ### Arguments
/// - `cmd`: The command identifier to send (e.g. `"new-tab"`, `"new-window"`)
///
/// ### Returns
/// - `true`: Another instance was found and the command was forwarded - caller should exit
/// - `false`: No existing instance is running - caller should continue
#[must_use]
pub fn try_send_command_to_existing_instance(cmd: &str) -> bool {
    match TcpStream::connect(("127.0.0.1", IPC_PORT)) {
        Ok(mut stream) => {
            let _ = writeln!(stream, "{CMD_PREFIX}{cmd}");
            log::info!("Single-instance: forwarded command '{cmd}' to running instance");
            true
        }
        Err(_) => false,
    }
}

/// Spawn a background thread that listens for messages from new Fulgur processes.
///
/// File-path lines are appended to `pending_files` so the render cycle can
/// open them, mirroring the macOS "Open With" path. Lines prefixed with
/// `CMD:` are appended to `pending_ipc_commands` so the render cycle can
/// dispatch in-process actions such as opening a new tab or window.
///
/// ### Arguments
/// - `pending_files`: Shared queue to receive file paths forwarded by other instances
/// - `pending_ipc_commands`: Shared queue to receive command strings forwarded by other instances
/// - `foreground_executor`: Executor used to wake the main thread so a
///   minimized window can be restored (its frame loop is paused while
///   minimized, so the render cycle would never drain the queues otherwise)
///
/// ### Returns
/// - `Some(Worker)`: The Drop-owned listener worker; dropping it stops the
///   listener (the wakeup self-connects to unblock the pending `accept`).
/// - `None`: The loopback port could not be bound.
#[must_use]
pub fn start_ipc_listener(
    pending_files: Arc<Mutex<Vec<PathBuf>>>,
    pending_ipc_commands: Arc<Mutex<Vec<String>>>,
    foreground_executor: ForegroundExecutor,
) -> Option<Worker> {
    let listener = match TcpListener::bind(("127.0.0.1", IPC_PORT)) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("Could not start single-instance IPC listener: {e}");
            return None;
        }
    };

    let worker = Worker::spawn(
        "fulgur-ipc-listener",
        IPC_WORKER_JOIN_TIMEOUT,
        move |shutdown| {
            for stream in listener.incoming() {
                if shutdown.load(Ordering::Relaxed) {
                    log::info!("IPC listener shutdown requested, stopping");
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let reader = BufReader::new(stream);
                        for line in reader.lines().map_while(Result::ok) {
                            if line.is_empty() {
                                continue;
                            }
                            if let Some(cmd) = line.strip_prefix(CMD_PREFIX) {
                                log::info!("IPC: received command '{cmd}'");
                                pending_ipc_commands.lock().push(cmd.to_string());
                                wake_minimized_windows(&foreground_executor);
                            } else {
                                let path = PathBuf::from(&line);
                                if path.exists() {
                                    log::info!(
                                        "IPC: queuing file from jump list: {}",
                                        path.display()
                                    );
                                    pending_files.lock().push(path);
                                    wake_minimized_windows(&foreground_executor);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("IPC listener accept error: {e}");
                    }
                }
            }
        },
    )
    .with_wakeup(|| {
        // Unblock the pending accept so the loop observes the shutdown flag.
        let _ = TcpStream::connect(("127.0.0.1", IPC_PORT));
    });
    Some(worker)
}

/// Wake the main thread to restore the minimized window that will process
/// the message just queued by the IPC listener.
///
/// While a window is minimized GPUI pauses its frame loop, so the render
/// cycle (which drains `pending_files` / `pending_ipc_commands`) never runs
/// and the window would stay hidden until the user restores it by hand.
/// Restoring the window resumes the frame loop, so the queued message is
/// processed on the next frame.
///
/// The window that processes the queues is the last-focused one (or any
/// window if none has been focused yet), so only that window is restored;
/// other minimized windows are left alone.
fn wake_minimized_windows(foreground_executor: &ForegroundExecutor) {
    foreground_executor
        .spawn(async move |cx: &mut App| {
            let last_focused = cx
                .try_global::<WindowManager>()
                .and_then(|wm| wm.get_last_focused());
            for handle in cx.windows() {
                let should_wake = last_focused.is_none_or(|id| handle.window_id() == id);
                if !should_wake {
                    continue;
                }
                let _ = handle.update(cx, |_, window, _| {
                    if window_activation::is_minimized(window) {
                        log::info!("IPC: window is minimized, restoring it");
                        window.activate_window();
                        window.request_animation_frame();
                    }
                });
            }
        })
        .detach();
}
