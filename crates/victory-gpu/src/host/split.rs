//! Splits of the submitting thread's work, for the frame line: a
//! submission's preparing in slices that add up ([`PREP_SPLIT`]), and the
//! refreshes' flushes by part ([`FLUSH_SPLIT`]). (Out of `host.rs`, which
//! the repository's policy keeps under 512 KiB.)

use super::*;

/// Since the last frame line: the nanoseconds of a submission's preparing
/// ([`Host::submit`], from its start to the lists sent, less its settles),
/// slice by slice, so that they add up to it: the write watch asked ahead
/// for the refreshes, the lists' steps by kind (title buffer copies, fills,
/// refreshes queued, shader writes declared, textures, other), the
/// refreshes' batch flushed, the asking's end, the snapshots, the card's
/// work before the lists; and the whole (the last).
pub(super) static PREP_SPLIT: [std::sync::atomic::AtomicU64; 14] = [const { std::sync::atomic::AtomicU64::new(0) }; 14];

/// [`PREP_SPLIT`]'s slots.
pub(super) mod prep {
    pub(crate) const ASK: usize = 0;
    /// Steps by kind: `STEPS + kind` ([`super::step_kind`]), six.
    pub(crate) const STEPS: usize = 1;
    pub(crate) const FLUSH: usize = 7;
    pub(crate) const ASK_END: usize = 8;
    pub(crate) const SNAPSHOTS: usize = 9;
    pub(crate) const CARD: usize = 10;
    pub(crate) const SETTLED: usize = 11;
    pub(crate) const WHOLE: usize = 12;
    /// After the preparing, not in it: the lists handed to the card.
    pub(crate) const HAND: usize = 13;
}

/// A step's kind for [`PREP_SPLIT`]: 0 a title buffer copy, 1 a fill, 2 a
/// refresh queued, 3 a shader write declared, 4 a texture, 5 other.
pub(super) fn step_kind(step: &Step) -> usize {
    match step {
        Step::Copy(..) => 0,
        Step::Fill(..) => 1,
        Step::Refresh(..) => 2,
        Step::GpuWrite(..) => 3,
        Step::Texture(..) => 4,
        _ => 5,
    }
}

pub(super) fn prep_add(slot: usize, d: std::time::Duration) {
    PREP_SPLIT[slot].fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
}

/// Since the last frame line, of the refreshes' flushes (`Batch::flush`):
/// 0 flushes, 1 refreshes they did (after joining); nanoseconds of 2 the
/// write watch asked ahead in them, 3 the journal (and the guards) asked
/// what changed ([`crate::watch::writes_into`]), 4 the changed pages copied
/// (`copy_stale`), and the updates whole: 5 of write-watched memory (3, 4
/// and the rest), 6 of memory not watched (copied whole).
pub(super) static FLUSH_SPLIT: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// Adds the time since `since` to part `i` of [`FLUSH_SPLIT`].
pub(crate) fn flush_split(i: usize, since: std::time::Instant) {
    FLUSH_SPLIT[i].fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// With the frame line: [`FLUSH_SPLIT`] per frame.
pub(super) fn flush_line(frames: f64) {
    let s: Vec<f64> = FLUSH_SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if s[0] == 0.0 {
        return;
    }
    tracing::info!(
        "the refreshes' flushes per frame: {:.0} flushes doing {:.0} refreshes; ms: write watch asked ahead {:.2}; updates of \
         watched memory {:.2} (the journal and guards asked {:.2}, changed pages copied {:.2}); updates of memory not watched, \
         copied whole, {:.2}",
        s[0],
        s[1],
        s[2] / 1e6,
        s[5] / 1e6,
        s[3] / 1e6,
        s[4] / 1e6,
        s[6] / 1e6
    );
}

/// With the frame line: [`PREP_SPLIT`] per frame.
pub(super) fn prep_line(frames: f64) {
    let s: Vec<f64> = PREP_SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames).collect();
    if s[prep::WHOLE] <= 0.0 {
        return;
    }
    let sum: f64 = s[..prep::SETTLED].iter().sum();
    tracing::info!(
        "preparing per frame, in slices that add up ({:.1} ms; settles taken out, {:.1} ms): write watch asked ahead {:.1};          steps: buffer copies {:.1}, fills {:.1}, refreshes queued {:.1}, shader writes declared {:.1}, textures {:.1},          other {:.1}; refreshes flushed {:.1}; asking's end {:.1}; snapshots {:.1}; card before the lists {:.1};          left {:.1}; then the lists handed to the card {:.1}",
        s[prep::WHOLE],
        s[prep::SETTLED],
        s[prep::ASK],
        s[prep::STEPS],
        s[prep::STEPS + 1],
        s[prep::STEPS + 2],
        s[prep::STEPS + 3],
        s[prep::STEPS + 4],
        s[prep::STEPS + 5],
        s[prep::FLUSH],
        s[prep::ASK_END],
        s[prep::SNAPSHOTS],
        s[prep::CARD],
        s[prep::WHOLE] - sum,
        s[prep::HAND]
    );
}

/// With the frame line, with `VICTORY_LIST_CALL_TIME=1`: the title's
/// threads inside our list calls, and the methods that took most.
pub(super) fn list_calls_line(frames: f64) {
    if crate::list::call_time_on() {
        let [calls, ns] = [0, 1].map(|i| crate::list::CALL_TIME[i].swap(0, Ordering::Relaxed) as f64 / frames);
        let mut by: Vec<(f64, f64, &str)> = crate::list::CALL_BY
            .iter()
            .zip(crate::list::CALL_NAMES)
            .map(|([n, t], name)| {
                (t.swap(0, Ordering::Relaxed) as f64 / frames, n.swap(0, Ordering::Relaxed) as f64 / frames, name)
            })
            .collect();
        by.sort_by(|a, b| b.0.total_cmp(&a.0));
        let top: Vec<String> = by
            .iter()
            .take(6)
            .map(|&(t, n, name)| format!("{name} {:.1} ms ({n:.0} calls, {:.1} us)", t / 1e6, if n > 0.0 { t / n / 1e3 } else { 0.0 }))
            .collect();
        tracing::info!(
            "the title's threads inside our list calls (VICTORY_LIST_CALL_TIME=1), per frame: {calls:.0} calls, {:.1} ms (all threads together; {:.2} us a call); most: {}",
            ns / 1e6,
            if calls > 0.0 { ns / calls / 1e3 } else { 0.0 },
            top.join(", ")
        );
    }
}
