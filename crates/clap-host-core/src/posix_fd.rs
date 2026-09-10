//! `clap.posix-fd-support`: fd registry + background poll thread.
//!
//! Plugins register file descriptors via the host callbacks in `host.rs`;
//! this module polls them on a dedicated thread and queues due events that
//! `host::pump_main_thread` delivers through `clap_plugin_posix_fd_support`
//! on the main thread. The plugin owns its descriptors — this module only
//! borrows them for `polling` and never closes them.

#![cfg(unix)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    clap_plugin_posix_fd_support, clap_posix_fd_flags,
};
use clap_sys::plugin::clap_plugin;
use crossbeam_queue::ArrayQueue;
use polling::{Event, Events, Poller};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::unix::io::{BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::Duration;

const DUE_CAPACITY: usize = 256;
const EVENTS_CAPACITY: usize = 64;
const WAIT_TIMEOUT: Duration = Duration::from_millis(50);

/// Stop flag for the poll thread; never set today, kept for teardown.
static WAKE_STOP: AtomicBool = AtomicBool::new(false);
static INIT: Once = Once::new();
static REG: Mutex<Option<Registry>> = Mutex::new(None);
static DUE_Q: LazyLock<ArrayQueue<(i32, clap_posix_fd_flags)>> =
    LazyLock::new(|| ArrayQueue::new(DUE_CAPACITY));

struct Registry {
    poller: Arc<Poller>,
    fds: HashMap<RawFd, clap_posix_fd_flags>,
}

fn event_for(fd: i32, flags: clap_posix_fd_flags) -> Option<Event> {
    let key = usize::try_from(fd).ok()?;
    if flags & (CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE) == 0 {
        return None;
    }
    Some(Event::new(
        key,
        flags & CLAP_POSIX_FD_READ != 0,
        flags & CLAP_POSIX_FD_WRITE != 0,
    ))
}

/// Lazily create the poller and spawn the poll thread. Called once from
/// `host::make_host`, and defensively from every registration entry point.
pub fn init_poll_thread() {
    INIT.call_once(|| {
        let Ok(poller) = Poller::new() else {
            return;
        };
        let poller = Arc::new(poller);
        if let Ok(mut reg) = REG.lock() {
            *reg = Some(Registry {
                poller: poller.clone(),
                fds: HashMap::new(),
            });
        }
        spawn_poll_thread(poller);
    });
}

fn spawn_poll_thread(poller: Arc<Poller>) {
    std::thread::spawn(move || {
        let mut events =
            Events::with_capacity(NonZeroUsize::new(EVENTS_CAPACITY).expect("non-zero"));
        while !WAKE_STOP.load(Ordering::Acquire) {
            events.clear();
            if poller.wait(&mut events, Some(WAIT_TIMEOUT)).is_err() {
                // Avoid a busy loop on a persistent wait error; the sleep
                // doubles as the stop-check cadence.
                std::thread::sleep(WAIT_TIMEOUT);
                continue;
            }
            for ev in events.iter() {
                let Ok(fd) = i32::try_from(ev.key) else {
                    continue;
                };
                let mut flags = 0;
                if ev.readable {
                    flags |= CLAP_POSIX_FD_READ;
                }
                if ev.writable {
                    flags |= CLAP_POSIX_FD_WRITE;
                }
                if flags == 0 {
                    flags = CLAP_POSIX_FD_ERROR;
                }
                let _ = DUE_Q.push((fd, flags));
            }
        }
    });
}

/// Register interest in `fd`. Fails if the fd is already registered, invalid,
/// the flags request neither read nor write interest, or the poller rejected
/// it.
pub fn register_fd(fd: i32, flags: clap_posix_fd_flags) -> bool {
    init_poll_thread();
    let Some(event) = event_for(fd, flags) else {
        return false;
    };
    let Ok(mut guard) = REG.lock() else {
        return false;
    };
    let Some(reg) = guard.as_mut() else {
        return false;
    };
    if reg.fds.contains_key(&fd) {
        return false;
    }
    // Safety: `fd` is borrowed, never owned; `unregister_fd` deletes it from
    // the poller before the plugin may close it.
    if unsafe { reg.poller.add(fd, event) }.is_err() {
        return false;
    }
    reg.fds.insert(fd, flags);
    // Wake the poll thread so an already-ready fd is picked up without
    // waiting for the next timeout tick.
    let _ = reg.poller.notify();
    true
}

/// Change the watched event kinds for a previously registered `fd`. Fails if
/// the fd is not registered or the flags request neither read nor write
/// interest.
pub fn modify_fd(fd: i32, flags: clap_posix_fd_flags) -> bool {
    init_poll_thread();
    let Some(event) = event_for(fd, flags) else {
        return false;
    };
    let Ok(mut guard) = REG.lock() else {
        return false;
    };
    let Some(reg) = guard.as_mut() else {
        return false;
    };
    if !reg.fds.contains_key(&fd) {
        return false;
    }
    // Safety: borrowed fd, see `register_fd`.
    if unsafe { reg.poller.modify(BorrowedFd::borrow_raw(fd), event) }.is_err() {
        return false;
    }
    reg.fds.insert(fd, flags);
    true
}

/// Remove `fd` from the poller. Fails if the fd was not registered.
pub fn unregister_fd(fd: i32) -> bool {
    init_poll_thread();
    let Ok(mut guard) = REG.lock() else {
        return false;
    };
    let Some(reg) = guard.as_mut() else {
        return false;
    };
    if reg.fds.remove(&fd).is_none() {
        return false;
    }
    // The fd may already be gone from the poller (e.g. after HUP); the
    // registry is the source of truth, so ignore poller errors here.
    let _ = unsafe { reg.poller.delete(BorrowedFd::borrow_raw(fd)) };
    true
}

/// Deliver queued fd events through the plugin's `on_fd`, on the calling
/// (main) thread. Events for fds the plugin already unregistered are dropped.
pub fn drain_due(plugin: *const clap_plugin) {
    if DUE_Q.is_empty() {
        return;
    }
    let ext = match crate::loader::plugin_ext(plugin, CLAP_EXT_POSIX_FD_SUPPORT) {
        Some(ext) => ext,
        None => {
            while DUE_Q.pop().is_some() {}
            return;
        }
    };
    let ext = unsafe { &*ext.cast::<clap_plugin_posix_fd_support>() };
    let Some(on_fd) = ext.on_fd else {
        while DUE_Q.pop().is_some() {}
        return;
    };
    while let Some((fd, flags)) = DUE_Q.pop() {
        let still_registered = REG
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|reg| reg.fds.contains_key(&fd)))
            .unwrap_or(false);
        if still_registered {
            unsafe { on_fd(plugin, fd, flags) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn register_twice_fails() {
        init_poll_thread();
        let (a, _b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        assert!(register_fd(fd, CLAP_POSIX_FD_READ));
        assert!(!register_fd(fd, CLAP_POSIX_FD_READ));
        assert!(unregister_fd(fd));
    }

    #[test]
    fn register_without_read_or_write_fails() {
        init_poll_thread();
        let (a, _b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        assert!(!register_fd(fd, 0));
        assert!(!register_fd(fd, CLAP_POSIX_FD_ERROR));
        assert!(!modify_fd(fd, 0));
        assert!(register_fd(fd, CLAP_POSIX_FD_READ));
        assert!(unregister_fd(fd));
    }

    #[test]
    fn unregister_unknown_fails() {
        init_poll_thread();
        assert!(!unregister_fd(1_000_000));
    }

    #[test]
    fn modify_unknown_fails() {
        init_poll_thread();
        assert!(!modify_fd(1_000_000, CLAP_POSIX_FD_WRITE));
    }
}
