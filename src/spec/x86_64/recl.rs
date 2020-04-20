//! EBR based garbage collector.

use once_cell::sync::Lazy;
use once_cell::unsync::Lazy as UnsyncLazy;
use std::mem::{align_of, size_of, take};
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static GUARDIAN_SLEEP_DURATION: Duration = Duration::from_millis(100);

#[inline(always)]
pub fn enter_critical() {
    PARTICIPANT_HANDLE.with(|key| {
        key.enter_critical();
    });
}

#[inline(always)]
pub fn exit_critical() {
    PARTICIPANT_HANDLE.with(|key| {
        key.exit_critical();
    });
}

/// Execute a closure in protected mode. This permits it to load protected pointers.
#[inline(always)]
pub fn protected<T>(f: impl FnOnce() -> T) -> T {
    PARTICIPANT_HANDLE.with(|key| {
        key.enter_critical();
        let r = f();
        key.exit_critical();
        r
    })
}

/// Defer a function.
#[inline(always)]
pub fn defer(f: impl FnOnce()) {
    let deferred = Deferred::new(f);
    PARTICIPANT_HANDLE.with(|key| key.defer(deferred));
}

fn guardian_thread_fn(gc: Arc<Global>) {
    loop {
        thread::sleep(GUARDIAN_SLEEP_DURATION);
        gc.collect();
    }
}

static GC: Lazy<Arc<Global>> = Lazy::new(|| {
    let state = Arc::new(Global::new());
    let state2 = Arc::clone(&state);
    thread::spawn(|| guardian_thread_fn(state2));
    state
});

thread_local! {
    pub static PARTICIPANT_HANDLE: UnsyncLazy<TSLocal> = UnsyncLazy::new(|| TSLocal::new(Arc::clone(&GC)));
}

pub struct TSLocal {
    local: Box<Local>,
}

impl TSLocal {
    fn new(global: Arc<Global>) -> TSLocal {
        let local = Box::new(Local::new(Arc::clone(&global)));
        let local_ptr = &*local as *const Local;
        global.add_local(local_ptr);
        Self { local }
    }
}

impl Deref for TSLocal {
    type Target = Local;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

impl Drop for TSLocal {
    fn drop(&mut self) {
        let global = Arc::clone(&self.local.global);
        let local_ptr = &*self.local as *const Local;
        global.remove_local(local_ptr);
    }
}

struct Deferred {
    call: fn([usize; 4]),
    task: [usize; 4],
}

fn deferred_exec_external(mut task: [usize; 4]) {
    unsafe {
        let fat_ptr: *mut dyn FnOnce() = ptr::read(&mut task as *mut [usize; 4] as usize as _);
        let boxed = Box::from_raw(fat_ptr);
        boxed();
    }
}

fn deferred_exec_internal<F: FnOnce()>(mut task: [usize; 4]) {
    unsafe {
        let f: F = ptr::read(task.as_mut_ptr() as *mut F);
        f();
    }
}

impl Deferred {
    fn new<'a, F: FnOnce() + 'a>(f: F) -> Self {
        let size = size_of::<F>();
        let align = align_of::<F>();
        unsafe {
            if size < size_of::<[usize; 4]>() && align <= align_of::<[usize; 4]>() {
                let mut task = [0; 4];
                ptr::write(task.as_mut_ptr() as *mut F, f);
                Self {
                    task,
                    call: deferred_exec_internal::<F>,
                }
            } else {
                let boxed: Box<dyn FnOnce() + 'a> = Box::new(f);
                let fat_ptr = Box::into_raw(boxed);
                let mut task = [0; 4];
                ptr::write(&mut task as *mut [usize; 4] as usize as _, fat_ptr);
                Self {
                    task,
                    call: deferred_exec_external,
                }
            }
        }
    }

    #[inline(always)]
    fn run(self) {
        (self.call)(self.task);
    }
}

unsafe impl Send for Deferred {}
unsafe impl Sync for Deferred {}

struct Global {
    // Global epoch. This value is always 0, 1 or 2.
    epoch: AtomicUsize,

    // List of participants.
    locals: Mutex<Vec<*const Local>>,
}

unsafe impl Send for Global {}
unsafe impl Sync for Global {}

#[inline(always)]
fn increment_epoch(a: &AtomicUsize) -> usize {
    loop {
        let current = a.load(Ordering::Acquire);
        let next = (current + 1) % 3;
        if a.compare_and_swap(current, next, Ordering::AcqRel) == current {
            break next;
        }
    }
}

impl Global {
    fn new() -> Self {
        Self {
            epoch: AtomicUsize::new(0),
            locals: Mutex::new(Vec::new()),
        }
    }

    fn add_local(&self, local: *const Local) {
        self.locals.lock().unwrap().push(local);
    }

    fn remove_local(&self, local: *const Local) {
        self.locals
            .lock()
            .unwrap()
            .retain(|maybe_this| *maybe_this != local);
    }

    fn collect(&self) {
        let start_global_epoch = self.epoch.load(Ordering::Acquire);
        let locals = self.locals.lock().unwrap();
        let mut local_lists = Vec::new();
        for local_ptr in &*locals {
            unsafe {
                let local = &**local_ptr;
                local_lists.push(&local.deferred);
                if local.active.load(Ordering::Acquire) > 0
                    && local.epoch.load(Ordering::Acquire) != start_global_epoch
                {
                    return;
                }
            }
        }
        drop(locals);
        if start_global_epoch != self.epoch.load(Ordering::Acquire) {
            return;
        }
        let next = increment_epoch(&self.epoch);
        for local_deferred_l in local_lists {
            let mut local_deferred = local_deferred_l.lock().unwrap();
            let to_collect = take(&mut local_deferred[next]);
            drop(local_deferred);
            for deferred in to_collect {
                deferred.run();
            }
        }
    }
}

pub struct Local {
    // Active flag.
    active: AtomicUsize,

    // Local epoch. This value is always 0, 1 or 2.
    epoch: AtomicUsize,

    // Reference to global state.
    global: Arc<Global>,

    // Objects marked for deletetion.
    deferred: Mutex<[Vec<Deferred>; 3]>,
}

impl Local {
    fn new(global: Arc<Global>) -> Self {
        Self {
            active: AtomicUsize::new(0),
            epoch: AtomicUsize::new(0),
            global,
            deferred: Mutex::new([Vec::new(), Vec::new(), Vec::new()]),
        }
    }

    #[inline(always)]
    pub fn enter_critical(&self) {
        if self.active.fetch_add(1, Ordering::Relaxed) == 0 {
            let global_epoch = self.global.epoch.load(Ordering::Relaxed);
            self.epoch.store(global_epoch, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub fn exit_critical(&self) {
        #[cfg(debug_assertions)]
        {
            if self.active.fetch_sub(1, Ordering::Relaxed) == 0 {
                panic!("uh oh");
            }
        }

        #[cfg(not(debug_assertions))]
        self.active.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline(always)]
    fn defer(&self, f: Deferred) {
        let global_epoch = self.global.epoch.load(Ordering::Relaxed);
        let mut deferred = self
            .deferred
            .lock()
            .unwrap_or_else(|_| std::process::abort());

        deferred[global_epoch].push(f);
    }
}
