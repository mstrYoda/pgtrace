use pgrx::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Number of slots in the shared-memory ring buffer.
pub const QUEUE_SIZE: usize = 1024;
/// Maximum serialized span batch per slot (bytes).
pub const MAX_PAYLOAD_SIZE: usize = 8192;

/// Circular queue in shared memory protected by an internal spinlock.
/// All fields are POD so that Postgres zero-initialisation is valid.
#[repr(C)]
pub struct SharedQueue {
    pub lock: AtomicBool,
    pub write_idx: AtomicUsize,
    pub read_idx: AtomicUsize,
    pub dropped: AtomicUsize,
    pub buffer: [u8; QUEUE_SIZE * MAX_PAYLOAD_SIZE],
    pub lengths: [usize; QUEUE_SIZE],
}

pub fn shared_queue_size() -> usize {
    std::mem::size_of::<SharedQueue>()
}

pub struct QueueStats {
    pub size: usize,
    pub dropped: usize,
}

// ─────────────────────────────────────────────────────────────
// Queue lifecycle
// ─────────────────────────────────────────────────────────────

static mut SHARED_QUEUE: *mut SharedQueue = std::ptr::null_mut();

pub unsafe fn set_queue(q: *mut SharedQueue) {
    SHARED_QUEUE = q;
}

pub unsafe fn get_queue() -> *mut SharedQueue {
    SHARED_QUEUE
}

/// Allocate or attach the shared queue via ShmemInitStruct.
/// Called once from the shmem_startup_hook.
pub unsafe fn init_queue() -> *mut SharedQueue {
    let mut found = false;
    let queue = pg_sys::ShmemInitStruct(
        "pg_otel_tracer_queue\0".as_ptr() as *const u8,
        shared_queue_size() as _,
        &mut found as *mut bool,
    ) as *mut SharedQueue;

    if !found {
        // Postgres has already zeroed the memory; we just need to
        // initialise the atomics so their vtable pointers are valid.
        std::ptr::addr_of_mut!((*queue).lock).write(AtomicBool::new(false));
        std::ptr::addr_of_mut!((*queue).write_idx).write(AtomicUsize::new(0));
        std::ptr::addr_of_mut!((*queue).read_idx).write(AtomicUsize::new(0));
        std::ptr::addr_of_mut!((*queue).dropped).write(AtomicUsize::new(0));
    }

    queue
}

unsafe fn lock_queue(q: *mut SharedQueue) {
    while (*q).lock.compare_exchange_weak(
        false,
        true,
        Ordering::Acquire,
        Ordering::Relaxed,
    ).is_err() {
        std::hint::spin_loop();
    }
}

unsafe fn unlock_queue(q: *mut SharedQueue) {
    (*q).lock.store(false, Ordering::Release);
}

/// Push a serialized span batch into the queue.
/// Returns `true` on success, `false` if the queue is full.
pub unsafe fn queue_push(queue: *mut SharedQueue, data: &[u8]) -> bool {
    if queue.is_null() || data.len() > MAX_PAYLOAD_SIZE {
        return false;
    }

    lock_queue(queue);
    let q = &mut *queue;

    let write_idx = q.write_idx.load(Ordering::Relaxed);
    let read_idx = q.read_idx.load(Ordering::Relaxed);

    if write_idx.wrapping_sub(read_idx) >= QUEUE_SIZE {
        q.dropped.fetch_add(1, Ordering::Relaxed);
        unlock_queue(queue);
        return false;
    }

    let slot = write_idx % QUEUE_SIZE;
    let offset = slot * MAX_PAYLOAD_SIZE;

    std::ptr::copy_nonoverlapping(
        data.as_ptr(),
        q.buffer.as_ptr().add(offset) as *mut u8,
        data.len(),
    );
    q.lengths[slot] = data.len();
    q.write_idx.store(write_idx.wrapping_add(1), Ordering::Relaxed);

    unlock_queue(queue);
    true
}

/// Pop a serialized span batch from the queue.
/// Returns `None` if the queue is empty.
pub unsafe fn queue_pop(queue: *mut SharedQueue) -> Option<Vec<u8>> {
    if queue.is_null() {
        return None;
    }

    lock_queue(queue);
    let q = &mut *queue;

    let read_idx = q.read_idx.load(Ordering::Relaxed);
    let write_idx = q.write_idx.load(Ordering::Relaxed);

    if read_idx == write_idx {
        unlock_queue(queue);
        return None;
    }

    let slot = read_idx % QUEUE_SIZE;
    let offset = slot * MAX_PAYLOAD_SIZE;
    let len = q.lengths[slot];

    let mut data = vec![0u8; len];
    std::ptr::copy_nonoverlapping(
        q.buffer.as_ptr().add(offset),
        data.as_mut_ptr(),
        len,
    );

    q.read_idx.store(read_idx.wrapping_add(1), Ordering::Relaxed);
    unlock_queue(queue);

    Some(data)
}

/// Return approximate queue statistics (best-effort without locking).
pub fn queue_stats() -> QueueStats {
    unsafe {
        let queue = get_queue();
        if queue.is_null() {
            return QueueStats { size: 0, dropped: 0 };
        }
        let q = &*queue;
        QueueStats {
            size: q.write_idx.load(Ordering::Relaxed)
                - q.read_idx.load(Ordering::Relaxed),
            dropped: q.dropped.load(Ordering::Relaxed),
        }
    }
}
