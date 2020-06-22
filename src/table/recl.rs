//! EBR based garbage collector.

use once_cell::sync::Lazy;
use once_cell::unsync::Lazy as UnsyncLazy;
use std::mem::{align_of, replace, size_of};
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static GUARDIAN_SLEEP_DURATION: Duration = Duration::from_millis(100);

pub fn enter_critical() {
    PARTICIPANT_HANDLE.with(|key| {
        key.enter_critical();
    });
}

pub fn exit_critical() {
    PARTICIPANT_HANDLE.with(|key| {
        key.exit_critical();
    });
}

/// Execute a closure in protected mode. This permits it to load protected pointers.
pub fn protected<T>(f: impl FnOnce() -> T) -> T {
    PARTICIPANT_HANDLE.with(|key| {
        key.enter_critical();
        let r = f();
        key.exit_critical();
        r
    })
}

/// Defer a function.
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
    local: Arc<Local>,
}

impl TSLocal {
    fn new(global: Arc<Global>) -> TSLocal {
        let local = Arc::new(Local::new(Arc::clone(&global)));
        global.add_local(local.clone());
        Self { local }
    }
}

impl Deref for TSLocal {
    type Target = Local;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

#[cfg(test)]
mod tests {
    use super::Deferred;

    #[test]
    fn defer_external() {
        let a = [61; 32];
        let deferred = Deferred::new(|| println!("{:?}", a));
        deferred.run();
    }
}

struct Deferred {
    call: fn([usize; 4]),
    task: [usize; 4],
}

fn deferred_exec_external(mut task: [usize; 4]) {
    // # Safety
    // This is safe as long as the deferred function is properly encoded in the task data.
    // Which it is as long as it's only called by `Deferred`.
    unsafe {
        let fat_ptr: *mut dyn FnOnce() = ptr::read(&mut task as *mut [usize; 4] as usize as _);
        let boxed = Box::from_raw(fat_ptr);
        boxed();
    }
}

fn deferred_exec_internal<F: FnOnce()>(mut task: [usize; 4]) {
    // # Safety
    // This is safe as long as the deferred function is properly encoded in the task data.
    // Which it is as long as it's only called by `Deferred`.
    unsafe {
        let f: F = ptr::read(task.as_mut_ptr() as *mut F);
        f();
    }
}

impl Deferred {
    fn new<'a, F: FnOnce() + 'a>(f: F) -> Self {
        let size = size_of::<F>();
        let align = align_of::<F>();

        // # Safety
        // Here we do some cursed trickery to pack a closure into a usize array.
        // If it doesn't fit we allocate it seperately and store the pointer in the array instead.
        // Because size and align was determined earlier in this function this is always safe.
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

    fn run(self) {
        (self.call)(self.task);
    }
}

struct Global {
    // Global epoch. This value is always 0, 1 or 2.
    epoch: AtomicUsize,

    // List of participants.
    locals: Mutex<Vec<Arc<Local>>>,
}

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

    fn add_local(&self, local: Arc<Local>) {
        self.locals.lock().unwrap().push(local);
    }

    fn collect(&self) {
        PARTICIPANT_HANDLE.with(|key| {
            UnsyncLazy::force(key);
        });

        let start_global_epoch = self.epoch.load(Ordering::Acquire);
        let mut locals = self.locals.lock().unwrap();
        let mut local_lists = Vec::new();
        for local_ptr in &*locals {
            let local = &**local_ptr;
            local_lists.push(&local.deferred);
            if local.active.load(Ordering::Acquire) > 0
                && local.epoch.load(Ordering::Acquire) != start_global_epoch
            {
                return;
            }
        }
        if start_global_epoch != self.epoch.load(Ordering::Acquire) {
            return;
        }
        let next = increment_epoch(&self.epoch);
        for local_deferred_l in local_lists {
            let mut local_deferred = local_deferred_l.lock().unwrap();
            let to_collect = replace(&mut local_deferred[next], Vec::new());
            drop(local_deferred);
            for deferred in to_collect {
                deferred.run();
            }
        }

        locals.retain(|arc| Arc::strong_count(arc) > 1)
    }
}

pub struct Local {
    // Active flag.
    active: AtomicUsize,

    // Local epoch. This value is always 0, 1 or 2.
    epoch: AtomicUsize,

    // Reference to global state.
    global: Arc<Global>,

    // Objects marked for deletion.
    deferred: Mutex<[Vec<Deferred>; 3]>,
}

impl Drop for Local {
    fn drop(&mut self) {
        let mut deferred = self.deferred.lock().unwrap();

        for i in 0..3 {
            for deferred in replace(&mut deferred[i], Vec::new()) {
                deferred.run();
            }
        }
    }
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

    fn enter_critical(&self) {
        if self.active.fetch_add(1, Ordering::Relaxed) == 0 {
            let global_epoch = self.global.epoch.load(Ordering::Relaxed);
            self.epoch.store(global_epoch, Ordering::Relaxed);
        }
    }

    fn exit_critical(&self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }

    fn defer(&self, f: Deferred) {
        let global_epoch = self.global.epoch.load(Ordering::Relaxed);
        let mut deferred = self
            .deferred
            .lock()
            .unwrap_or_else(|_| std::process::abort());

        deferred[global_epoch].push(f);
    }
}