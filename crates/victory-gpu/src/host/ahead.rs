//! Refreshes ahead across submissions (`VICTORY_PREWARM=<threads>`; off by
//! default; needs `VICTORY_ASYNC_SUBMIT=1`). See `docs/pipeline-b.md`.
//!
//! With the submissions prepared by our thread, the title hands a frame
//! over in one burst: while our thread prepares the first submissions in
//! order, the later ones wait in the queues' lines. Their refreshes of host
//! copies and textures are made ahead here, on helper threads, and made
//! again in order by `Host::submit` as before: a refresh copies only what
//! was written since its copy last asked (the journal, `copied[page]`), so
//! the in-order refresh finds little left and loses nothing.
//!
//! The one rule: nothing is refreshed ahead that a list being prepared
//! (its preparing began, it was not sent yet) may write after its own
//! refreshes: its title buffer copies' destinations, the ranges its
//! shaders may write, and, if it has a step that writes title memory
//! nobody knows where (`Run`), everything it refreshes. Its lists must
//! read those bytes as its own preparing leaves them. What it only reads
//! is refreshed ahead freely: title memory there is what it was when the
//! list refreshed it (the title gave the frame over and stands still). A
//! list not being prepared yet has its refreshes made in order after any
//! made ahead, which then find only what changed since. `Host::submit`
//! marks its lists as being prepared and then waits for every refresh
//! ahead given before that mark, which did not know it.

use super::*;

/// Helper threads (`VICTORY_PREWARM`, 0: off), with
/// `VICTORY_ASYNC_SUBMIT`.
pub(crate) fn prewarm() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        let n: usize = std::env::var("VICTORY_PREWARM").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0).min(16);
        if n == 0 || !crate::queue::behind() {
            return 0;
        }
        tracing::info!("refreshes ahead across submissions on {n} threads (VICTORY_PREWARM={n})");
        for i in 0..n {
            let _ = std::thread::Builder::new().name(format!("victory-prewarm-{i}")).spawn(helper);
        }
        n
    })
}

thread_local! {
    /// This thread refreshes ahead: its time is not the flushes'
    /// ([`super::flush_split`]).
    pub(super) static HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What a list given and not sent may write of title memory, as far as a
/// refresh ahead is concerned ([`given`]).
#[derive(Default)]
struct Marks {
    /// Title memory its steps write (`Copy` destinations) and its shaders
    /// may write (`GpuWrite`).
    writes: Vec<(usize, usize)>,
    /// Title memory it refreshes (`Refresh`, `Texture`).
    reads: Vec<(usize, usize)>,
    /// It has a `Run` step, which writes title memory nobody knows where:
    /// what it reads counts as written.
    run: bool,
}

/// The lists given and not sent ([`given`]).
#[derive(Default)]
struct Table {
    /// The last list's number.
    seq: u64,
    /// Per list number: what it may write.
    marks: std::collections::HashMap<u64, Marks>,
    /// What the lists being prepared may write (their `writes`, and their
    /// `reads` too where they have a `Run`), in order and joined: a refresh
    /// ahead touching it is left to the order.
    blocked: Vec<(usize, usize)>,
    /// Host list (its command list's address) -> its number.
    lists: std::collections::HashMap<usize, u64>,
    /// Numbers of lists whose refreshes ahead are not done.
    pending: std::collections::BTreeSet<u64>,
    /// Numbers of lists whose preparing in order began ([`wait`]) and that
    /// were not sent yet: what they may write is not refreshed ahead.
    started: std::collections::HashSet<u64>,
    /// Since the last line ([`S_LISTS`]..).
    stats: [u64; 10],
}

static TABLE: std::sync::LazyLock<Mutex<Table>> = std::sync::LazyLock::new(Default::default);
static DONE: std::sync::Condvar = std::sync::Condvar::new();

fn table() -> std::sync::MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Table {
    /// [`Table::blocked`] made again from the lists being prepared.
    fn block(&mut self) {
        self.blocked.clear();
        for s in &self.started {
            if let Some(m) = self.marks.get(s) {
                join_ranges(&mut self.blocked, m.writes.iter().copied());
                if m.run {
                    join_ranges(&mut self.blocked, m.reads.iter().copied());
                }
            }
        }
    }
}

// [`Table::stats`]' places: lists given, refreshes and textures, made
// ahead, held by a list being prepared, copies not write-watched, not in
// the asking aside (view memory and such), the same item twice in one job;
// microseconds of the helpers, of `Host::submit`'s waits, and the waits.
const S_LISTS: usize = 0;
const S_ITEMS: usize = 1;
const S_MADE: usize = 2;
const S_HELD: usize = 3;
const S_UNWATCHED: usize = 4;
const S_NOT_ASIDE: usize = 5;
const S_TWICE: usize = 6;
const S_HELPER_US: usize = 7;
const S_WAIT_US: usize = 8;
const S_WAITS: usize = 9;

/// One thing to make ahead.
enum Item {
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    Texture(std::sync::Arc<crate::texture::Staging>),
}

/// The refreshes ahead of some lists, and their numbers.
struct Job {
    seqs: Vec<u64>,
    items: Vec<Item>,
}

static JOBS: (Mutex<std::collections::VecDeque<Job>>, std::sync::Condvar) =
    (Mutex::new(std::collections::VecDeque::new()), std::sync::Condvar::new());

fn key(list: &HostList) -> usize {
    list.list.as_raw() as usize
}

/// The title gave a queue `lists` (title lists, closed), with
/// `VICTORY_ASYNC_SUBMIT`: they are registered, and what of them no list
/// given before holds is refreshed ahead.
pub(crate) fn given(lists: &[crate::object::This]) {
    if prewarm() == 0 {
        return;
    }
    let mut job = Job { seqs: Vec::new(), items: Vec::new() };
    // (host copy or staging buffer, from, to) of the job's items.
    let mut queued: std::collections::HashSet<(usize, usize, usize)> = std::collections::HashSet::new();
    let mut t = table();
    let t = &mut *t;
    for &l in lists {
        let Some(list) = crate::list::host_list(l) else {
            continue;
        };
        if t.lists.contains_key(&key(list)) {
            continue;
        }
        t.seq += 1;
        let seq = t.seq;
        t.lists.insert(key(list), seq);
        t.stats[S_LISTS] += 1;
        let mut marks = Marks::default();
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, _, len) => marks.writes.push((dst, dst + len)),
                Step::Run(_) => marks.run = true,
                Step::GpuWrite(_, at, len) => marks.writes.push((*at, *at + *len)),
                Step::Refresh(shadow, at, len) => {
                    let (from, to) = (*at, *at + *len);
                    marks.reads.push((from, to));
                    t.stats[S_ITEMS] += 1;
                    if touches(&t.blocked, from, to) {
                        t.stats[S_HELD] += 1;
                    } else if shadow.unwatched() {
                        t.stats[S_UNWATCHED] += 1;
                    } else if !queued.insert((std::sync::Arc::as_ptr(shadow) as usize, from, to)) {
                        t.stats[S_TWICE] += 1;
                    } else {
                        job.items.push(Item::Refresh(shadow.clone(), from, to));
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    marks.reads.push((at, at + len));
                    t.stats[S_ITEMS] += 1;
                    if touches(&t.blocked, at, at + len) {
                        t.stats[S_HELD] += 1;
                    } else if !queued.insert((std::sync::Arc::as_ptr(staging) as usize, 0, 0)) {
                        t.stats[S_TWICE] += 1;
                    } else {
                        job.items.push(Item::Texture(staging.clone()));
                    }
                }
                _ => {}
            }
        }
        t.marks.insert(seq, marks);
        job.seqs.push(seq);
    }
    if job.items.is_empty() {
        return;
    }
    t.pending.extend(job.seqs.iter().copied());
    JOBS.0.lock().unwrap_or_else(|e| e.into_inner()).push_back(job);
    JOBS.1.notify_one();
}

fn helper() {
    crate::framelog::keep_on_fast_cores("victory-prewarm");
    HERE.set(true);
    loop {
        let job = {
            let mut jobs = JOBS.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = JOBS.1.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        let began = std::time::Instant::now();
        let made = run(&job.items);
        let mut t = table();
        t.stats[S_MADE] += made;
        t.stats[S_NOT_ASIDE] += job.items.len() as u64 - made;
        t.stats[S_HELPER_US] += began.elapsed().as_micros() as u64;
        for s in &job.seqs {
            t.pending.remove(s);
        }
        drop(t);
        DONE.notify_all();
    }
}

/// Makes `items` ahead: one asking of the kernel for all of them, aside
/// from the submission's own (`crate::watch::ask_aside`); an item the
/// asking does not hold (view memory, memory not write-watched) is left to
/// the preparing in order. How many were made.
fn run(items: &[Item]) -> u64 {
    let ranges: Vec<(usize, usize)> = items
        .iter()
        .map(|item| match item {
            Item::Refresh(_, from, to) => (*from, *to),
            Item::Texture(staging) => {
                let (at, len) = staging.memory();
                (at, at + len)
            }
        })
        .collect();
    let Some(_aside) = crate::watch::ask_aside(&ranges) else {
        return 0;
    };
    let mut made = 0;
    for (item, &(from, to)) in items.iter().zip(&ranges) {
        if !crate::watch::aside_holds(from, to) {
            continue;
        }
        match item {
            Item::Refresh(shadow, from, to) => {
                shadow.update(*from, to - from);
            }
            Item::Texture(staging) => staging.refresh(),
        }
        made += 1;
    }
    made
}

/// Before `runs` are prepared: they count as being prepared (what they
/// hold is refreshed ahead no more), and every refresh ahead given until
/// now, which did not know that, is done first.
pub(crate) fn wait(runs: &[&HostList]) {
    if prewarm() == 0 {
        return;
    }
    let mut t = table();
    let seqs: Vec<u64> = runs.iter().filter_map(|r| t.lists.get(&key(r)).copied()).collect();
    if !seqs.is_empty() {
        t.started.extend(seqs);
        t.block();
    }
    let mark = t.seq;
    if t.pending.first().is_none_or(|&s| s > mark) {
        return;
    }
    let began = std::time::Instant::now();
    while t.pending.first().is_some_and(|&s| s <= mark) {
        t = DONE.wait(t).unwrap_or_else(|e| e.into_inner());
    }
    t.stats[S_WAIT_US] += began.elapsed().as_micros() as u64;
    t.stats[S_WAITS] += 1;
}

/// `runs` were prepared (their refreshes done, their snapshots taken):
/// what they held is free for refreshes ahead.
pub(crate) fn sent(runs: &[&HostList]) {
    if prewarm() == 0 {
        return;
    }
    let mut t = table();
    let t = &mut *t;
    let seqs: Vec<u64> = runs.iter().filter_map(|r| t.lists.remove(&key(r))).collect();
    if seqs.is_empty() {
        return;
    }
    for s in &seqs {
        t.started.remove(s);
        t.marks.remove(s);
    }
    t.block();
}

/// With the frame line.
pub(crate) fn line(frames: f64) {
    if prewarm() == 0 {
        return;
    }
    let s = std::mem::take(&mut table().stats).map(|v| v as f64 / frames);
    tracing::info!(
        "refreshes ahead across submissions (VICTORY_PREWARM), per frame: {:.0} lists given, {:.0} refreshes and textures, \
         {:.0} made ahead; left to the order: {:.0} touching what a list being prepared may write, {:.0} of copies not write-watched, \
         {:.0} outside the asking aside (view memory and such), {:.0} the same item twice in one hand-over; {:.2} ms on \
         the helpers; Host::submit waited {:.2} ms in {:.1} waits",
        s[S_LISTS],
        s[S_ITEMS],
        s[S_MADE],
        s[S_HELD],
        s[S_UNWATCHED],
        s[S_NOT_ASIDE],
        s[S_TWICE],
        s[S_HELPER_US] / 1e3,
        s[S_WAIT_US] / 1e3,
        s[S_WAITS]
    );
}
