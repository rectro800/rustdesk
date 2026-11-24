//! Audio threads run above the video capture, encoding and decoding work that competes with them
//! for the CPU, so a busy machine delays the picture instead of breaking up the sound.

use std::cell::Cell;

thread_local! {
    static RAISED: Cell<bool> = const { Cell::new(false) };
}

/// Best effort, and cheap to call again on the same thread, e.g. from every audio callback.
pub fn raise_current_thread() {
    if RAISED.with(|raised| raised.replace(true)) {
        return;
    }
    if let Err(err) = raise() {
        hbb_common::log::info!("Audio thread keeps its normal priority: {err}");
    }
}

#[cfg(windows)]
fn raise() -> std::io::Result<()> {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
        .map_err(|err| std::io::Error::other(err.to_string()))
}

#[cfg(target_os = "linux")]
fn raise() -> std::io::Result<()> {
    use hbb_common::libc;
    // Linux applies a nice value set through a thread id to that thread alone. Lowering it needs
    // CAP_SYS_NICE or a raised RLIMIT_NICE, so an ordinary desktop user may be refused.
    const AUDIO_NICE: libc::c_int = -10;
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, AUDIO_NICE) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn raise() -> std::io::Result<()> {
    Ok(())
}
