//! Single-instance guard: a second launch asks the running instance to show
//! its window and exits, so two processes never share the profile databases.

/// Outcome of [`acquire`].
pub enum Instance {
    /// This process owns the instance; keep the guard alive for the whole run.
    Primary(Guard),
    /// Another instance is already running.
    Secondary,
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use super::Instance;

    type Handle = isize;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const WAIT_OBJECT_0: u32 = 0;
    const EVENT_MODIFY_STATE: u32 = 0x0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
        fn CreateEventW(
            attrs: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> Handle;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> Handle;
        fn SetEvent(h: Handle) -> i32;
        fn WaitForMultipleObjects(
            count: u32,
            handles: *const Handle,
            wait_all: i32,
            millis: u32,
        ) -> u32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn mutex_name(name: &str) -> Vec<u16> {
        wide(&format!("Local\\WaClient-{name}"))
    }

    fn event_name(name: &str) -> Vec<u16> {
        wide(&format!("Local\\WaClient-{name}-activate"))
    }

    /// Owns the instance mutex and the activation listener.
    pub struct Guard {
        mutex: Handle,
        activate: Handle,
        stop: Handle,
        listeners: Mutex<Vec<JoinHandle<()>>>,
    }

    impl Guard {
        /// Runs `f` on a background thread each time a second launch signals
        /// this instance. The thread ends when the guard is dropped.
        pub fn on_activate(&self, f: impl Fn() + Send + 'static) {
            let handles = [self.activate, self.stop];
            let join = std::thread::spawn(move || {
                loop {
                    // SAFETY: both handles stay open until the guard has joined this thread.
                    let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                    if r == WAIT_OBJECT_0 {
                        f();
                    } else {
                        break;
                    }
                }
            });
            self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(join);
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: handles were created by acquire and are closed exactly once, here.
            unsafe {
                SetEvent(self.stop);
            }
            let joins = std::mem::take(self.listeners.get_mut().unwrap_or_else(|e| e.into_inner()));
            for j in joins {
                let _ = j.join();
            }
            unsafe {
                CloseHandle(self.stop);
                CloseHandle(self.activate);
                CloseHandle(self.mutex);
            }
        }
    }

    pub fn acquire(name: &str) -> Instance {
        // SAFETY: the name is a NUL-terminated UTF-16 string that outlives the call.
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name(name).as_ptr()) };
        let err = unsafe { GetLastError() };
        // A null mutex (creation failed) falls through as Primary: running
        // unguarded beats refusing to start. CloseHandle(0) in Drop is harmless.
        if mutex != 0 && err == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(mutex) };
            return Instance::Secondary;
        }
        let activate = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name(name).as_ptr()) };
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        Instance::Primary(Guard {
            mutex,
            activate,
            stop,
            listeners: Mutex::new(Vec::new()),
        })
    }

    pub fn signal_primary(name: &str) -> bool {
        // SAFETY: as above; the opened handle is closed before returning.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE, 0, event_name(name).as_ptr());
            if h == 0 {
                return false;
            }
            let ok = SetEvent(h) != 0;
            CloseHandle(h);
            ok
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Instance;

    pub struct Guard;

    impl Guard {
        pub fn on_activate(&self, _f: impl Fn() + Send + 'static) {}
    }

    pub fn acquire(_name: &str) -> Instance {
        Instance::Primary(Guard)
    }

    pub fn signal_primary(_name: &str) -> bool {
        false
    }
}

pub use imp::Guard;

/// Claims the named instance, or reports that another process holds it.
pub fn acquire(name: &str) -> Instance {
    imp::acquire(name)
}

/// Asks the running instance to show itself. False when none is listening.
pub fn signal_primary(name: &str) -> bool {
    imp::signal_primary(name)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn unique(tag: &str) -> String {
        format!("test-{tag}-{}", std::process::id())
    }

    #[test]
    fn second_acquire_is_secondary_until_guard_drops() {
        let name = unique("acquire");
        let first = acquire(&name);
        assert!(matches!(first, Instance::Primary(_)));
        assert!(matches!(acquire(&name), Instance::Secondary));
        drop(first);
        assert!(matches!(acquire(&name), Instance::Primary(_)));
    }

    #[test]
    fn signal_triggers_activate_callback() {
        let name = unique("signal");
        let Instance::Primary(guard) = acquire(&name) else {
            panic!("expected primary");
        };
        let (tx, rx) = mpsc::channel();
        guard.on_activate(move || {
            let _ = tx.send(());
        });
        assert!(signal_primary(&name));
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn signal_without_primary_returns_false() {
        assert!(!signal_primary(&unique("none")));
    }

    #[test]
    fn dropping_guard_stops_listener() {
        let name = unique("drop");
        let Instance::Primary(guard) = acquire(&name) else {
            panic!("expected primary");
        };
        guard.on_activate(|| {});
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let start = Instant::now();
            drop(guard);
            let _ = tx.send(start.elapsed());
        });
        let elapsed = rx.recv_timeout(Duration::from_secs(1)).expect("drop hung");
        assert!(elapsed < Duration::from_secs(1));
        t.join().unwrap();
    }
}
