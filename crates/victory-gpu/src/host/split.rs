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
    if crate::shadow::refresh_split() > 1 {
        let [flushes, pieces, bytes] =
            [0, 1, 2].map(|i| crate::shadow::SPLIT_COUNT[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "refreshes' copies split (VICTORY_REFRESH_SPLIT={}), per frame: {flushes:.0} flushes in {pieces:.0} pieces, {:.1} MB",
            crate::shadow::refresh_split(),
            bytes / 1e6
        );
    }
    if flush_overlap() {
        let [none, left] = [0, 1].map(|i| OVERLAP[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "title copies at submission (VICTORY_FLUSH_OVERLAP=1), per frame: {none:.0} flushed no refresh, {left:.0} refreshes left pending past one"
        );
    }
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
    crate::critical::line();
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

/// Whether a title buffer copy at submission flushes only the pending
/// refreshes whose range touches the bytes it writes
/// (`VICTORY_FLUSH_OVERLAP=1`; off by default: every pending refresh, as
/// before). The order that matters is theirs alone: a refresh recorded
/// before the copy reads those bytes as they were. The others stay pending
/// for the submission's last flush, or the next one that needs them, and
/// read title memory the copy did not change. A flush before every such
/// copy was hundreds of small flushes a frame, each asking the write watch
/// and the guards again.
pub(crate) fn flush_overlap() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FLUSH_OVERLAP").is_some_and(|v| v == "1"))
}

/// Since the last frame line, with [`flush_overlap`]: title copies that
/// flushed nothing, and refreshes left pending past a title copy.
pub(super) static OVERLAP: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Batch {
    /// Before a title buffer copy that writes `[at, end)`: the pending
    /// refreshes are done, or with [`flush_overlap`] only those whose range
    /// touches it.
    pub(super) fn flush_before_copy(&mut self, at: usize, end: usize) {
        if !flush_overlap() {
            self.flush();
            return;
        }
        let (touching, rest) = touching(std::mem::take(&mut self.refreshes), at, end);
        OVERLAP[1].fetch_add(rest.len() as u64, Ordering::Relaxed);
        if touching.is_empty() {
            OVERLAP[0].fetch_add(1, Ordering::Relaxed);
            self.refreshes = rest;
            return;
        }
        self.refreshes = touching;
        self.flush();
        // (`flush` emptied the index with the list.)
        self.refreshes = rest;
        for (at, r) in self.refreshes.iter().enumerate() {
            self.index.insert(std::sync::Arc::as_ptr(&r.0) as usize, at);
        }
    }
}

/// Pending refreshes `(copy, from, to)` split into those whose range
/// touches `[at, end)` and the others, each in the order given
/// ([`Batch::flush_before_copy`]).
fn touching<T>(refreshes: Vec<(T, usize, usize)>, at: usize, end: usize) -> (Vec<(T, usize, usize)>, Vec<(T, usize, usize)>) {
    refreshes.into_iter().partition(|r| r.1 < end && at < r.2)
}

#[cfg(test)]
mod tests {
    use super::touching;

    /// A refresh touches a copy's bytes if their ranges share a byte: one
    /// that ends where the copy begins, or begins where it ends, does not;
    /// one inside, around or over either end does. Order is kept.
    #[test]
    fn only_refreshes_sharing_a_byte_with_the_copy_are_flushed() {
        let pending = vec![
            ('a', 0x1000, 0x2000), // ends where the copy begins
            ('b', 0x1800, 0x2400), // over its start
            ('c', 0x2100, 0x2200), // inside
            ('d', 0x1000, 0x9000), // around
            ('e', 0x2f00, 0x3100), // over its end
            ('f', 0x3000, 0x4000), // begins where it ends
            ('g', 0x8000, 0x9000), // far
        ];
        let (flushed, left) = touching(pending, 0x2000, 0x3000);
        assert_eq!(flushed.iter().map(|r| r.0).collect::<String>(), "bcde");
        assert_eq!(left.iter().map(|r| r.0).collect::<String>(), "afg");
        let (flushed, left) = touching(vec![((), 0x1000, 0x2000)], 0x5000, 0x5000);
        assert!(flushed.is_empty(), "an empty copy touches nothing");
        assert_eq!(left.len(), 1);
    }
}
