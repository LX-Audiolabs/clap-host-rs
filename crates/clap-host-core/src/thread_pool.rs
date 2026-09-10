//! Host-side `clap.thread-pool`: fixed worker pool for `request_exec`.

use crate::loader;
use clap_sys::ext::thread_pool::{CLAP_EXT_THREAD_POOL, clap_plugin_thread_pool};
use clap_sys::plugin::clap_plugin;
use std::num::NonZero;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, Once};

static INIT: Once = Once::new();
static CURRENT_PLUGIN: AtomicPtr<clap_plugin> = AtomicPtr::new(ptr::null_mut());
static POOL: Mutex<PoolState> = Mutex::new(PoolState {
    next_index: AtomicU32::new(0),
    wake_count: 0,
    done_count: 0,
    stop: false,
});
static WORK_CV: Condvar = Condvar::new();
static DONE_CV: Condvar = Condvar::new();
static WORKER_COUNT: AtomicU32 = AtomicU32::new(0);
static EXT_MISSING_WARNED: AtomicBool = AtomicBool::new(false);

struct PoolState {
    next_index: AtomicU32,
    wake_count: u32,
    done_count: u32,
    stop: bool,
}

fn plugin_pool_ext(plugin: *const clap_plugin) -> Option<&'static clap_plugin_thread_pool> {
    loader::plugin_ext(plugin, CLAP_EXT_THREAD_POOL).map(|p| unsafe { &*p.cast() })
}

fn run_exec(plugin: *const clap_plugin, task_index: u32) {
    if let Some(ext) = plugin_pool_ext(plugin)
        && let Some(exec) = ext.exec
    {
        unsafe { exec(plugin, task_index) };
    }
}

fn worker_loop() {
    let mut guard = POOL.lock().unwrap();
    loop {
        while guard.wake_count == 0 && !guard.stop {
            guard = WORK_CV.wait(guard).unwrap();
        }
        if guard.stop {
            return;
        }
        // Claim-Loop (Race-Note): `wake_count` zählt Tasks, nicht Worker —
        // auch wenn num_tasks > worker_count, zieht jeder Worker so lange
        // Tasks, bis nichts mehr zu holen ist.
        while guard.wake_count > 0 {
            guard.wake_count -= 1;
            let task_index = guard.next_index.fetch_add(1, Ordering::SeqCst);
            drop(guard);
            run_exec(CURRENT_PLUGIN.load(Ordering::SeqCst), task_index);
            guard = POOL.lock().unwrap();
            guard.done_count += 1;
            DONE_CV.notify_one();
        }
    }
}

pub fn init() {
    INIT.call_once(|| {
        let n = std::thread::available_parallelism()
            .map(NonZero::get)
            .unwrap_or(1)
            .max(1);
        for i in 0..n {
            std::thread::Builder::new()
                .name(format!("clap-host-pool-{i}"))
                .spawn(worker_loop)
                .expect("spawn clap-host-pool worker");
        }
        WORKER_COUNT.store(n as u32, Ordering::SeqCst);
    });
}

pub fn set_current_plugin(plugin: *const clap_plugin) {
    CURRENT_PLUGIN.store(plugin as *mut clap_plugin, Ordering::SeqCst);
}

pub fn request_exec(num_tasks: u32) -> bool {
    if num_tasks == 0 {
        return true;
    }
    let plugin = CURRENT_PLUGIN.load(Ordering::SeqCst);
    if plugin.is_null() {
        return false;
    }
    let Some(ext) = plugin_pool_ext(plugin) else {
        if !EXT_MISSING_WARNED.swap(true, Ordering::SeqCst) {
            eprintln!("clap.thread-pool: plugin has no thread-pool extension");
        }
        return false;
    };
    if ext.exec.is_none() {
        return false;
    }
    if num_tasks == 1 {
        run_exec(plugin, 0);
        return true;
    }
    init();
    let mut guard = POOL.lock().unwrap();
    guard.next_index.store(0, Ordering::SeqCst);
    guard.done_count = 0;
    guard.wake_count = num_tasks;
    WORK_CV.notify_all();
    while guard.done_count < num_tasks {
        guard = DONE_CV.wait(guard).unwrap();
    }
    true
}

pub fn worker_count() -> usize {
    WORKER_COUNT.load(Ordering::SeqCst) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::plugin::clap_plugin;
    use std::ffi::{CStr, c_char, c_void};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Tests teilen sich die globalen Statics (EXEC_COUNT, SEEN_INDEXES,
    // CURRENT_PLUGIN, POOL); ohne Serialisierung laufen sie sich beim
    // Parallel-Testlauf gegenseitig über. Abweichung vom Brief: Tests
    // unverändert, aber jeder Test hält TEST_LOCK.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    static EXEC_COUNT: AtomicU32 = AtomicU32::new(0);
    static SEEN_INDEXES: StdMutex<Vec<u32>> = StdMutex::new(Vec::new());

    unsafe extern "C" fn fake_exec(_: *const clap_plugin, task_index: u32) {
        EXEC_COUNT.fetch_add(1, Ordering::SeqCst);
        SEEN_INDEXES.lock().unwrap().push(task_index);
    }

    static FAKE_POOL: clap_plugin_thread_pool = clap_plugin_thread_pool {
        exec: Some(fake_exec),
    };

    unsafe extern "C" fn fake_get_extension(
        _: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_THREAD_POOL {
            ptr::from_ref(&FAKE_POOL).cast()
        } else {
            ptr::null()
        }
    }

    fn fake_plugin() -> clap_plugin {
        clap_plugin {
            get_extension: Some(fake_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        }
    }

    #[test]
    fn request_exec_zero_is_ok_without_work() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        EXEC_COUNT.store(0, Ordering::SeqCst);
        let p = fake_plugin();
        set_current_plugin(&raw const p);
        assert!(request_exec(0));
        assert_eq!(EXEC_COUNT.load(Ordering::SeqCst), 0);
        set_current_plugin(ptr::null());
    }

    #[test]
    fn request_exec_one_runs_on_caller() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        EXEC_COUNT.store(0, Ordering::SeqCst);
        SEEN_INDEXES.lock().unwrap().clear();
        let p = fake_plugin();
        set_current_plugin(&raw const p);
        assert!(request_exec(1));
        assert_eq!(EXEC_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(*SEEN_INDEXES.lock().unwrap(), vec![0]);
        set_current_plugin(ptr::null());
    }

    #[test]
    fn request_exec_fans_out_unique_indexes() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        EXEC_COUNT.store(0, Ordering::SeqCst);
        SEEN_INDEXES.lock().unwrap().clear();
        let p = fake_plugin();
        set_current_plugin(&raw const p);
        let n = 4u32;
        assert!(request_exec(n));
        assert_eq!(EXEC_COUNT.load(Ordering::SeqCst), n);
        let mut idxs = SEEN_INDEXES.lock().unwrap().clone();
        idxs.sort_unstable();
        assert_eq!(idxs, (0..n).collect::<Vec<_>>());
        set_current_plugin(ptr::null());
    }

    #[test]
    fn request_exec_more_tasks_than_workers() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        EXEC_COUNT.store(0, Ordering::SeqCst);
        SEEN_INDEXES.lock().unwrap().clear();
        let p = fake_plugin();
        set_current_plugin(&raw const p);
        // Claim-Loop-Pfad: mehr Tasks als Worker — jeder Worker zieht sich
        // nacheinander mehrere Tasks. n wird zur Laufzeit aus worker_count
        // abgeleitet, um den Pfad maschinenunabhängig zu erzwingen.
        let n = (worker_count() * 2) as u32;
        assert!(n > 1, "pool should have at least 2 workers for this test");
        assert!(request_exec(n));
        assert_eq!(EXEC_COUNT.load(Ordering::SeqCst), n);
        let mut idxs = SEEN_INDEXES.lock().unwrap().clone();
        idxs.sort_unstable();
        assert_eq!(idxs, (0..n).collect::<Vec<_>>());
        set_current_plugin(ptr::null());
    }

    unsafe extern "C" fn null_get_extension(
        _: *const clap_plugin,
        _: *const c_char,
    ) -> *const c_void {
        ptr::null()
    }

    #[test]
    fn request_exec_false_without_plugin_ext() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        let p = clap_plugin {
            get_extension: Some(null_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        };
        set_current_plugin(&raw const p);
        assert!(!request_exec(2));
        set_current_plugin(ptr::null());
    }

    #[test]
    fn request_exec_false_without_current_plugin() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        set_current_plugin(ptr::null());
        assert!(!request_exec(2));
    }

    static FAKE_POOL_NO_EXEC: clap_plugin_thread_pool = clap_plugin_thread_pool { exec: None };

    unsafe extern "C" fn no_exec_get_extension(
        _: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_THREAD_POOL {
            ptr::from_ref(&FAKE_POOL_NO_EXEC).cast()
        } else {
            ptr::null()
        }
    }

    #[test]
    fn request_exec_false_when_exec_is_none() {
        let _g = TEST_LOCK.lock().unwrap();
        init();
        let p = clap_plugin {
            get_extension: Some(no_exec_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        };
        set_current_plugin(&raw const p);
        assert!(!request_exec(2));
        set_current_plugin(ptr::null());
    }
}
