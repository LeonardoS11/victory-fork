# Pipeline (b): refreshes ahead across submissions

Goal: the ~36 ms per frame of a submission's preparing that does not depend on
order (refreshes of host copies, textures) is done on helper threads while
the submitting thread prepares the earlier submissions in order. With
`VICTORY_ASYNC_SUBMIT=1` the title hands the whole frame over in one burst
(queue.rs, `join_parts`), so the frame's later submissions sit in the queue
lines while the worker prepares the first ones.

## Idea: speculative refresh, redone in order

A refresh is idempotent and incremental. It copies the pages written since
this copy last asked, using the write watch's journal, and `copied[page]`
holds the asking number per page under the copy's own lock. A refresh made
early ("ahead") and the in-order refresh of the same range later together
give the same result as the in-order one alone:

- what was written in between (a step of an earlier submission, a
  copy-back) is found by the in-order refresh through the journal and copied
  again;
- what the early refresh copied is found unchanged.

So the in-order pass keeps every guarantee it has now. The work moves ahead
only for ranges no one wrote in between, which is most of them.

## The one rule: nothing a list being prepared reads

(First version, target110: nothing any earlier unsent list held. Run AI3:
17,800 of 21,400 items a frame left to the order, 3,600 made ahead. Now
only lists whose preparing began count, see the end of this section.)

Hazard: submission i, still not sent, refreshed range R (or will), then a
step of i writes title memory in R (a title buffer copy). The flush before
the copy exists so that i's lists read R as it was. A refresh ahead of a
later j, run between that flush and i's lists reaching the card, would
copy the new bytes into R under i's lists.

Rule: at the moment a submission is given to the queue (title thread,
`queue::submit_cut`), its items are registered in a table: per host copy, the
ranges of its `Refresh` and `GpuWrite` steps; per staging buffer, its
`Texture` steps. Only items that overlap nothing registered by another
submission still in the table are refreshed ahead. An entry leaves the table
when its submission has been sent (after `take_snapshots` in
`Host::submit`).

Across queues, registration order may differ from execution order.
`Host::submit` therefore waits, before preparing, for every refresh ahead of
a submission registered no later than its own (sequence numbers). A refresh
ahead never runs beside the preparing of a submission it could have
disturbed.

Items left out:

- copies not write-watched (copied whole);
- ranges shaders write: `Shadow::update` already skips `pending`, and `GpuWrite`
  ranges are registered.

## Cost and the risk that decides it

A refresh ahead made item by item asks the kernel per item, under the
journal's lock taken alone. That is the contention that made
`VICTORY_WIDE_REFRESH` worthless in AF3-b (36-42 ms summed over threads for
a wall time of 18). The in-order thread reads the journal shared (RwLock,
`read_answer`) only for ranges of its own submission's asking. Every kernel
asking ahead would stop it.

So a refresh ahead should ask once per submission: one kernel asking over
the spans of its items, recorded in the journal without touching the
in-order submission's `ahead` state. The items are then answered from the
journal under the shared lock. That needs a variant of `ask_ahead` that
returns its asking number, and a `Shadow::update` that takes the answer
("now" plus the pages) from it.

Decision point: AH3-b ("melhor" = WIDE_REFRESH + VIEWS_ONCE + RwLock ...).
If "refreshes flushed" falls clearly below 18 ms of wall time there, the
journal no longer stands in the way and the pipeline can go ahead as above.
If not, the journal's lock comes first: per-region locks, or recording
askings without the lock taken alone.

## Switches

`VICTORY_PREWARM=<threads>`: off by default; needs `VICTORY_ASYNC_SUBMIT=1`.
Lines: items registered, refreshed ahead, left out (overlap), µs on helpers,
wait in `Host::submit`. The time ahead is kept out of `FLUSH_SPLIT` by a
thread-local flag.

## Where the code goes

`host/ahead.rs` (host.rs is at 523.9 KB of the 524,288-byte limit). Calls in
host.rs: one wait and one release in `Host::submit`. One call in
`queue::submit_cut`.

## Rule as of target111

Only lists whose preparing began (`ahead::wait` marks them) and that were
not sent yet hold their ranges. `Host::submit` marks first, then waits for
every refresh ahead given before the mark. A list queued but not started
has its refreshes made again in order after any made ahead, so the bytes
it reads are those its own preparing leaves. The line splits what is left
to the order: held, not write-watched, outside the asking aside, twice in
one hand-over.
