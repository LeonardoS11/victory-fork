//! Who waits for whom in a frame (`VICTORY_CRITICAL_PATH=1`; off by
//! default): with the frame line, per frame on average,
//!
//! - the frame, present to present;
//! - the title's calls of `ExecuteCommandLists`: how many, the time inside
//!   them, and of it the wait for the submission lock (`Host::submit`), on
//!   which threads;
//! - the time inside `Present`;
//! - each thread that records lists: its time inside our list calls (the
//!   outermost only, `crate::list::call_timer`), its time between its
//!   first and last call of the frame outside them (the title's own work),
//!   and the rest of the frame (nothing recorded: waiting, or other work).
//!
//! A frame is cut at each present ([`frame_end`]); a thread's calls are
//! counted in the frame they end in. What the submitting thread's work is
//! made of is in the preparing's line (`crate::host`, slices).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

/// Whether it is on (`VICTORY_CRITICAL_PATH=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CRITICAL_PATH").is_some_and(|v| v == "1"))
}

/// Nanoseconds since the first look.
fn now() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

/// One thread's list calls in the frame so far.
struct Thread {
    id: u32,
    calls: AtomicU64,
    inside: AtomicU64,
    /// Its first call's start and last call's end in the frame
    /// (`u64::MAX` and 0: none).
    first: AtomicU64,
    last: AtomicU64,
    /// Summed over the frames since the line: inside, outside (between the
    /// first and last call), idle (the rest of those frames), calls, frames
    /// with calls.
    sums: [AtomicU64; 5],
}

static THREADS: Mutex<Vec<Arc<Thread>>> = Mutex::new(Vec::new());

thread_local! {
    static ME: std::cell::OnceCell<Arc<Thread>> = const { std::cell::OnceCell::new() };
}

fn thread_id() -> u32 {
    #[cfg(windows)]
    // SAFETY: no arguments.
    return unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    #[cfg(not(windows))]
    0
}

/// A list call of this thread ended now, after `ns` (the outermost of the
/// thread's calls).
pub(crate) fn list_call(ns: u64) {
    if !on() {
        return;
    }
    let end = now();
    ME.with(|me| {
        let t = me.get_or_init(|| {
            let t = Arc::new(Thread {
                id: thread_id(),
                calls: AtomicU64::new(0),
                inside: AtomicU64::new(0),
                first: AtomicU64::new(u64::MAX),
                last: AtomicU64::new(0),
                sums: [const { AtomicU64::new(0) }; 5],
            });
            THREADS.lock().unwrap_or_else(|e| e.into_inner()).push(t.clone());
            t
        });
        t.calls.fetch_add(1, Relaxed);
        t.inside.fetch_add(ns, Relaxed);
        t.first.fetch_min(end.saturating_sub(ns), Relaxed);
        t.last.fetch_max(end, Relaxed);
    });
}

/// Since the line: frames, their nanoseconds; execute calls, ns inside,
/// ns waiting for the submission lock; ns inside `Present`.
static TOTALS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Our submitting thread (`VICTORY_ASYNC_SUBMIT`, `crate::queue` worker)
/// was at work for `ns`.
pub(crate) fn worker(ns: u64) {
    if on() {
        TOTALS[6].fetch_add(ns, Relaxed);
    }
}

/// The threads that called `ExecuteCommandLists` since the line (ids).
static EXECUTERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// A title call of `ExecuteCommandLists` took `ns`.
pub(crate) fn execute(ns: u64) {
    if !on() {
        return;
    }
    TOTALS[2].fetch_add(1, Relaxed);
    TOTALS[3].fetch_add(ns, Relaxed);
    let id = thread_id();
    let mut e = EXECUTERS.lock().unwrap_or_else(|e| e.into_inner());
    if !e.contains(&id) {
        e.push(id);
    }
}

/// A submission waited `since` (till now) for the submission lock.
pub(crate) fn lock_waited(since: std::time::Instant) {
    if on() {
        TOTALS[4].fetch_add(since.elapsed().as_nanos() as u64, Relaxed);
    }
}

/// A call of `Present` took `ns`.
pub(crate) fn present(ns: u64) {
    if on() {
        TOTALS[5].fetch_add(ns, Relaxed);
    }
}

/// A frame ends: each recording thread's frame is summed and starts again.
pub(crate) fn frame_end() {
    if !on() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = now();
    let began = LAST.swap(now, Relaxed);
    if began == 0 {
        return;
    }
    let frame = now - began;
    TOTALS[0].fetch_add(1, Relaxed);
    TOTALS[1].fetch_add(frame, Relaxed);
    for t in THREADS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let calls = t.calls.swap(0, Relaxed);
        let inside = t.inside.swap(0, Relaxed);
        let (first, last) = (t.first.swap(u64::MAX, Relaxed), t.last.swap(0, Relaxed));
        if calls == 0 {
            continue;
        }
        let span = last.saturating_sub(first.max(began)).min(frame);
        t.sums[0].fetch_add(inside, Relaxed);
        t.sums[1].fetch_add(span.saturating_sub(inside), Relaxed);
        t.sums[2].fetch_add(frame.saturating_sub(span.max(inside)), Relaxed);
        t.sums[3].fetch_add(calls, Relaxed);
        t.sums[4].fetch_add(1, Relaxed);
    }
}

/// With the frame line: the averages per frame since the last.
pub(crate) fn line() {
    if !on() {
        return;
    }
    let t: Vec<u64> = TOTALS.iter().map(|v| v.swap(0, Relaxed)).collect();
    let frames = t[0].max(1) as f64;
    let ms = |ns: u64| ns as f64 / 1e6 / frames;
    let executers: Vec<String> = std::mem::take(&mut *EXECUTERS.lock().unwrap_or_else(|e| e.into_inner()))
        .iter()
        .map(|id| id.to_string())
        .collect();
    let mut threads: Vec<(u64, String)> = THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|th| {
            let [inside, outside, idle, calls, with] = [0, 1, 2, 3, 4].map(|i| th.sums[i].swap(0, Relaxed));
            (with > 0).then(|| {
                (
                    inside,
                    format!(
                        "thread {}: inside our calls {:.1} ms ({:.0} calls), its own work between them {:.1}, idle {:.1} (in {} of {} frames)",
                        th.id,
                        ms(inside),
                        calls as f64 / frames,
                        ms(outside),
                        ms(idle),
                        with,
                        t[0]
                    ),
                )
            })
        })
        .collect();
    threads.sort_by(|a, b| b.0.cmp(&a.0));
    tracing::info!(
        "critical path per frame (VICTORY_CRITICAL_PATH=1): frame {:.1} ms; ExecuteCommandLists {:.1} calls, {:.1} ms inside \
         on the title's threads (of it {:.1} waiting for the submission lock) on threads [{}]; our submitting thread at \
         work {:.1} ms (VICTORY_ASYNC_SUBMIT); Present {:.1} ms; recording threads: {}",
        ms(t[1]),
        t[2] as f64 / frames,
        ms(t[3]),
        ms(t[4]),
        executers.join(", "),
        ms(t[6]),
        ms(t[5]),
        threads.into_iter().take(8).map(|t| t.1).collect::<Vec<_>>().join("; ")
    );
}
