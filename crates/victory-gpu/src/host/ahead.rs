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
//! The one rule: nothing is refreshed ahead that a submission given
//! earlier and not sent yet reads or has shaders write (its `Refresh`,
//! `GpuWrite` and `Texture` steps). Its lists must read those bytes as its
//! own preparing leaves them, not as a later submission's. `Host::submit`
//! waits for the refreshes ahead of every submission given no later than
//! its own before it prepares, so that none runs beside the preparing of a
//! submission it could disturb (queues may run in another order than they
//! were given).

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

/// What a submission given and not sent holds, by list ([`given`]).
#[derive(Default)]
struct Table {
    /// The last list's number.
    seq: u64,
    /// Per host copy (its address): ranges read or written by shaders, with
    /// the number of the list that holds them.
    shadows: std::collections::HashMap<usize, Vec<(usize, usize, u64)>>,
    /// Per staging buffer: the lists that refresh it.
    stagings: std::collections::HashMap<usize, Vec<u64>>,
    /// Host list (its command list's address) -> its number.
    lists: std::collections::HashMap<usize, u64>,
    /// Numbers of lists whose refreshes ahead are not done.
    pending: std::collections::BTreeSet<u64>,
    /// Since the last line: lists given, refreshes and textures registered,
    /// made ahead, left out; microseconds of the helpers; of `Host::submit`'s
    /// waits.
    stats: [u64; 7],
}

static TABLE: std::sync::LazyLock<Mutex<Table>> = std::sync::LazyLock::new(Default::default);
static DONE: std::sync::Condvar = std::sync::Condvar::new();

fn table() -> std::sync::MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

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
        t.stats[0] += 1;
        for step in &list.deferred().copies {
            match step {
                Step::Refresh(shadow, at, len) | Step::GpuWrite(shadow, at, len) => {
                    let (from, to) = (*at, *at + *len);
                    let ranges = t.shadows.entry(std::sync::Arc::as_ptr(shadow) as usize).or_default();
                    let free = !ranges.iter().any(|&(a, b, s)| s != seq && a < to && from < b);
                    ranges.push((from, to, seq));
                    if let Step::Refresh(..) = step {
                        t.stats[1] += 1;
                        if free && !shadow.unwatched() {
                            job.items.push(Item::Refresh(shadow.clone(), from, to));
                        } else {
                            t.stats[3] += 1;
                        }
                    }
                }
                Step::Texture(staging) => {
                    let lists = t.stagings.entry(std::sync::Arc::as_ptr(staging) as usize).or_default();
                    let free = lists.iter().all(|&s| s == seq);
                    lists.push(seq);
                    t.stats[1] += 1;
                    if free {
                        job.items.push(Item::Texture(staging.clone()));
                    } else {
                        t.stats[3] += 1;
                    }
                }
                _ => {}
            }
        }
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
        t.stats[2] += made;
        t.stats[3] += job.items.len() as u64 - made;
        t.stats[4] += began.elapsed().as_micros() as u64;
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

/// Before `runs` are prepared: the refreshes ahead of every list given no
/// later than theirs are done.
pub(crate) fn wait(runs: &[&HostList]) {
    if prewarm() == 0 {
        return;
    }
    let mut t = table();
    let Some(last) = runs.iter().filter_map(|r| t.lists.get(&key(r)).copied()).max() else {
        return;
    };
    if t.pending.first().is_none_or(|&s| s > last) {
        return;
    }
    let began = std::time::Instant::now();
    while t.pending.first().is_some_and(|&s| s <= last) {
        t = DONE.wait(t).unwrap_or_else(|e| e.into_inner());
    }
    t.stats[5] += began.elapsed().as_micros() as u64;
    t.stats[6] += 1;
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
    t.shadows.retain(|_, ranges| {
        ranges.retain(|r| !seqs.contains(&r.2));
        !ranges.is_empty()
    });
    t.stagings.retain(|_, lists| {
        lists.retain(|s| !seqs.contains(s));
        !lists.is_empty()
    });
}

/// With the frame line.
pub(crate) fn line(frames: f64) {
    if prewarm() == 0 {
        return;
    }
    let s = std::mem::take(&mut table().stats).map(|v| v as f64 / frames);
    tracing::info!(
        "refreshes ahead across submissions (VICTORY_PREWARM), per frame: {:.0} lists given, {:.0} refreshes and textures, \
         {:.0} made ahead, {:.0} left to the order (held by an earlier list, or not asked aside); {:.2} ms on the helpers; \
         Host::submit waited {:.2} ms in {:.1} waits",
        s[0],
        s[1],
        s[2],
        s[3],
        s[4] / 1e3,
        s[5] / 1e3,
        s[6]
    );
}
