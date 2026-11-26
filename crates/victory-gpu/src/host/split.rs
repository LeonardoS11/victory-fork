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
    if loading_flights() > 0 {
        tracing::info!(
            "flights capped while loading (VICTORY_LOADING_FLIGHTS={}, after {} submissions in a frame): found full {} times since the last line",
            loading_flights(),
            loading_after(),
            LOADING_FULL.swap(0, Ordering::Relaxed)
        );
    }
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

/// `VICTORY_WIDE_REFRESH=<n>`, n of 2 or more: a flush's refreshes go to
/// the helper threads in `n` pieces of about equal bytes, not one job a
/// copy (`=1`); 0: not wide. (One job a copy measured no better, run 112:
/// every update took the journal's and the guards' locks, and a flush held
/// a few copies; with `VICTORY_VIEWS_ONCE` and `VICTORY_FLUSH_OVERLAP` a
/// flush holds many and an update takes the journal's lock only to read.)
pub(super) fn refresh_pieces() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_WIDE_REFRESH").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0).min(32))
}

/// A flush's refreshes as jobs for the helper threads ([`refresh_pieces`]).
pub(super) fn refresh_jobs(refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>) -> Vec<Box<dyn FnOnce() + Send>> {
    let total: usize = refreshes.iter().map(|r| r.2 - r.1).sum();
    let parts = if total < crate::shadow::wide_least() { 1 } else { refresh_pieces() };
    if parts < 2 {
        // (A small flush, or one job a copy as `=1` asks: see the caller.)
        if parts == 1 && refresh_pieces() >= 2 {
            return vec![Box::new(move || refreshes.into_iter().for_each(|(shadow, from, to)| Batch::update(&shadow, from, to)))];
        }
        return refreshes
            .into_iter()
            .map(|(shadow, from, to)| Box::new(move || Batch::update(&shadow, from, to)) as Box<dyn FnOnce() + Send>)
            .collect();
    }
    let share = total.div_ceil(parts).max(1);
    let mut jobs: Vec<Box<dyn FnOnce() + Send>> = Vec::with_capacity(parts);
    let (mut group, mut bytes) = (Vec::new(), 0);
    for r in refreshes {
        bytes += r.2 - r.1;
        group.push(r);
        if bytes >= share {
            let g = std::mem::take(&mut group);
            jobs.push(Box::new(move || g.into_iter().for_each(|(shadow, from, to)| Batch::update(&shadow, from, to))));
            bytes = 0;
        }
    }
    if !group.is_empty() {
        jobs.push(Box::new(move || group.into_iter().for_each(|(shadow, from, to)| Batch::update(&shadow, from, to))));
    }
    jobs
}

/// `VICTORY_TEXTURE_WIDE=<n>` (off by default; 0 or 1: off): the textures a
/// submission's steps bring up to date wait for the next flush of
/// refreshes (before any step of ours writes title memory, and at the
/// end), and are then refreshed on `n` threads at once. Each refresh is a
/// texture's own (its own lock and notes, the journal read under its
/// lock, a read buffer per thread): 6-8 ms a frame one after the other on
/// the submitting thread (Series S, run AB3).
pub(super) fn texture_wide() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_TEXTURE_WIDE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0).min(32))
}

impl Batch {
    /// A texture due: refreshed now, or with [`texture_wide`] at the next
    /// flush.
    pub(super) fn texture_now_or_later(&mut self, staging: &std::sync::Arc<crate::texture::Staging>) {
        if texture_wide() > 1 {
            self.later.push(staging.clone());
        } else {
            staging.refresh();
        }
    }

    /// The textures left for the flush ([`texture_wide`]), refreshed on the
    /// helper threads; this waits for all of them.
    pub(super) fn textures_later(&mut self) {
        if self.later.is_empty() {
            return;
        }
        let later = std::sync::Arc::new(std::mem::take(&mut self.later));
        // A few textures are refreshed here (waking the helper threads costs
        // more than they do).
        let parts = if later.len() < 4 { 1 } else { texture_wide().min(later.len()) };
        let jobs: Vec<Box<dyn FnOnce() + Send>> = (0..parts)
            .map(|part| {
                let later = later.clone();
                Box::new(move || later.iter().skip(part).step_by(parts).for_each(|s| s.refresh())) as Box<dyn FnOnce() + Send>
            })
            .collect();
        crate::shadow::wide_run(jobs);
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

/// The most submissions in the air while the title loads
/// (`VICTORY_LOADING_FLIGHTS=<n>`; 0, the default, no such cap): the card
/// was lost at the world's entry four times, the last with
/// `VICTORY_ASYNC_SUBMIT`, while a frame sent many more submissions than
/// the ~18 of play. A frame is taken as loading once it has sent more than
/// [`loading_after`] submissions.
pub(super) fn loading_flights() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_LOADING_FLIGHTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// Submissions in a frame after which it is loading
/// (`VICTORY_LOADING_AFTER`, 40).
pub(super) fn loading_after() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_LOADING_AFTER").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(40))
}

/// Times [`loading_full`] said full since the last frame line.
static LOADING_FULL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The frame last seen and the submissions started when it was first seen.
static LOADING_FRAME: Mutex<(u64, u64)> = Mutex::new((u64::MAX, 0));

/// Flights (submissions) among `line`'s items.
pub(super) fn runs(line: &[Late]) -> usize {
    line.iter().filter(|l| matches!(l, Late::Run(_))).count()
}

/// Whether `flights` in the air reach [`loading_flights`] in a frame that
/// is loading.
pub(super) fn loading_full(flights: usize) -> bool {
    let most = loading_flights();
    if most == 0 {
        return false;
    }
    // (Each submission comes here: the frame's first is noted.)
    let (frame, started) = (crate::frame::current(), STARTED.load(Ordering::Relaxed));
    let mut seen = LOADING_FRAME.lock().unwrap_or_else(|e| e.into_inner());
    if seen.0 != frame {
        *seen = (frame, started);
    }
    let full = flights >= most && started - seen.1 > loading_after();
    if full {
        LOADING_FULL.fetch_add(1, Ordering::Relaxed);
    }
    full
}
