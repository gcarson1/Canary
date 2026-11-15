//! A worker pool for the audio thread: runs the independent steps of one plan
//! level on several cores. The audio thread publishes a batch, takes tasks
//! from it itself, and waits only for tasks a worker has already claimed, so a
//! worker that is asleep or slow to wake never holds a block back.
//!
//! Claims go through one atomic word holding the batch's generation, its task
//! count and the next task index: a worker can only claim a task of the batch
//! that word names, and
//! the audio thread waits for every claimed task before it publishes the
//! next, so a claimed task's batch stays put while it runs.
//!
//! Workers spin briefly between batches, then nap; none of that
//! touches the audio thread, which never blocks, locks or allocates here.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// After a batch a worker spins this long (the next level usually follows at
/// once), then naps between checks, longer once the audio thread has been
/// quiet a while: a sleepy worker only means less help, never a late block.
const SPIN: Duration = Duration::from_micros(50);
const NAP: Duration = Duration::from_micros(250);
const QUIET: Duration = Duration::from_millis(100);
const LONG_NAP: Duration = Duration::from_millis(5);

/// Turns a panic through the pool into an abort: a task that unwound on a
/// worker would leave the audio thread waiting forever, and one unwinding the
/// audio thread would free what workers still use.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        if thread::panicking() {
            std::process::abort();
        }
    }
}
/// `state` is generation (high 32 bits), task count (16), next index (16).
const INDEX_BITS: u32 = 16;
const MASK: u64 = (1 << INDEX_BITS) - 1;
const MAX_TASKS: usize = MASK as usize;

fn pack(generation: u64, count: usize, index: usize) -> u64 {
    generation << (2 * INDEX_BITS) | (count as u64) << INDEX_BITS | index as u64
}

struct Shared {
    /// Generation, task count and next task index (`pack`).
    state: AtomicU64,
    /// The current batch, written before `state` moves to its generation.
    run: AtomicUsize,
    data: AtomicUsize,
    /// Tasks finished in the current batch.
    done: AtomicUsize,
    stop: AtomicBool,
}

impl Shared {
    /// Claims the next task of generation `generation`: its index, or `None`
    /// if the batch moved on or has no task left.
    fn claim(&self, generation: u64) -> Option<(usize, usize, usize)> {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let (count, i) = (
                (state >> INDEX_BITS & MASK) as usize,
                (state & MASK) as usize,
            );
            if state >> (2 * INDEX_BITS) != generation || i >= count {
                return None;
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // The batch's own: written before its generation was
                    // published, and not rewritten while this task is unfinished.
                    let run = self.run.load(Ordering::Relaxed);
                    let data = self.data.load(Ordering::Relaxed);
                    return Some((i, run, data));
                }
                Err(now) => state = now,
            }
        }
    }
}

type Task = unsafe fn(*const (), usize);

/// Threads that help the audio thread run a plan level in parallel.
pub struct WorkerPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// `workers` helper threads (0 runs everything on the calling thread).
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            state: AtomicU64::new(0),
            run: AtomicUsize::new(0),
            data: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let threads = (0..workers)
            .filter_map(|i| {
                let shared = shared.clone();
                thread::Builder::new()
                    .name(format!("canary-dsp-{i}"))
                    .spawn(move || worker(&shared))
                    .ok()
            })
            .collect();
        Self { shared, threads }
    }

    /// Helper threads running.
    pub fn workers(&self) -> usize {
        self.threads.len()
    }

    /// Runs `task(i)` for every `i` in `0..count`, on this thread and any
    /// worker that is awake, and returns once all have run. Allocation-free.
    /// `task` must be safe to call for different indices at once. Only one
    /// thread may call `run` at a time (`&mut self`).
    pub fn run<F: Fn(usize) + Sync>(&mut self, count: usize, task: &F) {
        if count <= 1 || self.threads.is_empty() || count > MAX_TASKS {
            (0..count).for_each(task);
            return;
        }
        unsafe fn call<F: Fn(usize) + Sync>(data: *const (), i: usize) {
            // SAFETY: `data` is the `&F` given to `run`, alive until it returns,
            // and `run` returns only once every claimed task is done.
            unsafe { (*data.cast::<F>())(i) }
        }
        let _guard = AbortOnUnwind;
        let s = &*self.shared;
        let run: Task = call::<F>;
        s.run.store(run as usize, Ordering::Relaxed);
        s.data
            .store(std::ptr::from_ref(task) as usize, Ordering::Relaxed);
        s.done.store(0, Ordering::Relaxed);
        let generation = ((s.state.load(Ordering::Relaxed) >> (2 * INDEX_BITS)) + 1) & 0xFFFF_FFFF;
        // Publishes the batch above (Release), from task 0.
        s.state.store(pack(generation, count, 0), Ordering::Release);
        while let Some((i, _, _)) = s.claim(generation) {
            task(i);
            s.done.fetch_add(1, Ordering::AcqRel);
        }
        // Every task is claimed; wait for the ones workers are running.
        while s.done.load(Ordering::Acquire) < count {
            std::hint::spin_loop();
        }
    }
}

fn worker(s: &Shared) {
    let _guard = AbortOnUnwind;
    set_realtime_priority();
    let mut seen = s.state.load(Ordering::Acquire) >> (2 * INDEX_BITS);
    let mut last = Instant::now();
    while !s.stop.load(Ordering::Relaxed) {
        let generation = s.state.load(Ordering::Acquire) >> (2 * INDEX_BITS);
        if generation != seen {
            seen = generation;
            while let Some((i, run, data)) = s.claim(generation) {
                // SAFETY: `run` and `data` are the batch's (claimed under its
                // generation), alive until `done` counts this task.
                unsafe {
                    let run: Task = std::mem::transmute::<usize, Task>(run);
                    run(data as *const (), i);
                }
                s.done.fetch_add(1, Ordering::AcqRel);
            }
            last = Instant::now();
            continue;
        }
        let idle = last.elapsed();
        if idle < SPIN {
            std::hint::spin_loop();
        } else if idle < QUIET {
            thread::sleep(NAP);
        } else {
            thread::sleep(LONG_NAP);
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Asks the OS to schedule this thread like interactive work, so a worker is
/// rarely preempted while it holds a task the audio thread waits for.
fn set_realtime_priority() {
    #[cfg(target_os = "macos")]
    {
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, relative: i32) -> i32;
        }
        // SAFETY: a plain libSystem call on the current thread.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_runs_once_whoever_runs_it() {
        let mut pool = WorkerPool::new(3);
        let hits: Vec<AtomicUsize> = (0..64).map(|_| AtomicUsize::new(0)).collect();
        for round in 0..5_000 {
            let count = 1 + round % 64;
            pool.run(count, &|i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
            });
        }
        let total: usize = hits.iter().map(|h| h.load(Ordering::Relaxed)).sum();
        assert_eq!(total, (0..5_000).map(|r| 1 + r % 64).sum::<usize>());
        // With nobody to help, the caller runs them all.
        let mut alone = WorkerPool::new(0);
        let n = AtomicUsize::new(0);
        alone.run(10, &|_| {
            n.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(n.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn the_audio_thread_never_allocates_publishing_a_batch() {
        let mut pool = WorkerPool::new(2);
        let n = AtomicUsize::new(0);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100 {
                pool.run(8, &|_| {
                    n.fetch_add(1, Ordering::Relaxed);
                });
            }
        });
        assert_eq!(n.load(Ordering::Relaxed), 800);
    }
}
