# Findings: recompute cost of Contribution-based Versions

## Question

Is ADR-0007 fast enough? Versions are computed from Contributions (ordered by Authority, then Reference time, then Recorded time, with ranked merge opt-in per Kind), every write cites an Episode, and Erasure deletes an Episode's Contributions, Citations and statements and then recomputes every Version it touched, closed ones included.

## Answer

Nothing here forces a change to ADR-0007: a patch write to a Node with 500 Contributions took about 10 ms at p50 and 22 to 36 ms at p95 for a bounded Valid-time interval, and Erasure of one Episode cited by 2,086 Versions across 500 Nodes took 24 to 35 seconds in one transaction, with reads unaffected.

Two costs are worth knowing before the schema is fixed. Erasure holds a lock on every affected Node for the whole transaction, so writes to those Nodes wait for it (22.7 and 33.2 seconds in the two concurrent runs). Erasure time also grows with the affected Nodes' history length. A patch with an open-ended Valid time on a 500-Contribution Node cost 200 to 540 ms in this implementation.

## Method

Everything ran on `postgres:17` in one scratch container (host port 55432), as the non-owner role `authenticated` under RLS. Each measured call was one transaction: `begin; set local role authenticated; select set_config('request.jwt.claims', ..., true); <call>; commit`. The schema follows the ADRs: four scalar `timestamptz` time columns plus a precision per Valid-time bound, half-open on both axes, exclusion constraints on `tstzrange(...)` expressions, B-tree indexes, a Tenant on every row with RLS policies calling `gnomon.tenant_id()`, foreign keys with `ON DELETE CASCADE` from Citations to Versions, an invoker write function, and a `SECURITY DEFINER` Erasure owned by a NOLOGIN role. A guard trigger lets callers only close an open Version at the transaction time. Recorded time is the transaction timestamp.

Dataset: 10 Tenants, 24,620 Nodes, 1,915,541 Version rows (572,830 open), 1,123,350 Contributions, 560,966 Episodes, 10.6 million Citations, 3.7 GB. Bulk Nodes have a mixed history (60% small, 30% medium, 10% large, about 60 Version rows each). Contributions use five years of Valid time (mean length about 60 days, 4% open-ended, 2% with an unknown start). Each Episode gets an Authority from 0 to 3, half the bulk Nodes use ranked merge, and about 5% of bulk Episodes state a null value that ends a property. The data are synthetic.

One recompute primitive serves every path. `apply_step` takes a Node, a recorded time and a Valid-time window. It reads the visible Contributions overlapping the window, cuts the window into segments, picks the winner per property (Authority if ranked, then Reference time, then Recorded time), merges adjacent segments with equal values, then closes the open Versions that no longer match and opens the new ones. A Citation goes to every overlapping Contribution that states the winning value. The write path calls it once at the transaction time. Erasure replays it once per recorded time that remains in the Node's history. The window grows to whole open Versions that overlap or touch it, so the result equals a full recompute.

Correctness checks, all passing: a full replay of a loaded Node reproduces its Versions and Citations exactly; an oracle that picks winners straight from the Contributions agreed with as-of reads on 600 random points on a small database and on 3,000 random points after every Erasure run (0 disagreements); the two Erasure strategies leave identical rows on a test Node; no Citation or Contribution of the erased Episode is left.

Writes (measurement 1): six cells, 10, 100 and 500 Contributions per Node with ranked merge off and on. Each cell has 250 measured Nodes and 20 warm-up Nodes, one patch write per Node (one Episode, one Valid-time interval, two properties), cells interleaved in random order in one session. Run on a fresh clone of the base each round.

Erasure (measurement 2): three target Episodes, each stating one property over a 45-day Valid-time window in 500 Nodes that already had at least 10 Episodes of history, written at an early, middle and late recorded time of those Nodes. The middle one (Tenant 1) is cited by 2,086 Versions in 500 Nodes, as in the brief. The early one (Tenant 9) is cited by 2,804 and the late one (Tenant 10) by 1,305. Two strategies: `full` truncates each Node's whole version history and replays every step from the first Contribution; `from_earliest` (the cheaper strategy, measurement 4) truncates and replays only from the earliest recorded time the erased Episode touched in that Node. Each run used a fresh clone. The "rows" columns count physical writes inside the function, and a before and after snapshot of the affected Nodes gives the semantic changes. Concurrency runs added 8 reader threads (half on affected Nodes), a monitor sampling `pg_locks` and `pg_stat_activity` every 250 ms, and one write to an affected Node and one to an unaffected Node of the same Tenant, both started 3 seconds into the Erasure.

As-of reads (measurement 3): point reads of one Node and batch reads of 50 Nodes at a random valid-at and known-at instant, on a clone with the Contribution, Episode and Citation tables dropped ("versions only"), on a full clone, and on a clone after a committed Erasure. The first two alternated twice.

Both write paths take `pg_advisory_xact_lock` on the Node (Erasure takes all affected Nodes' locks up front, in a fixed order), so a write cannot compute from Contributions that an Erasure is about to delete.

## Results

### 1. Write latency (ms, client wall time, scratch server with fsync off)

Two rounds on fresh clones. The table gives round 1, round 2 in each cell. "Bounded" excludes the 4% of patches with an open-ended Valid time.

| Contributions | Ranked merge | All p50 | All p95 | All p99 | Bounded p50 | Bounded p95 | Open-ended p50 (n=6 to 13) | Share of writes that changed a Version |
|---|---|---|---|---|---|---|---|---|
| 10 | off | 1.2, 1.4 | 2.3, 2.6 | 2.8, 3.9 | 1.2, 1.4 | 2.2, 2.4 | 2.0, 2.4 | 100% |
| 10 | on | 1.4, 1.6 | 2.5, 2.9 | 3.6, 4.5 | 1.3, 1.6 | 2.1, 2.5 | 3.2, 3.2 | 100% |
| 100 | off | 3.4, 3.6 | 6.5, 8.3 | 37, 43 | 3.3, 3.5 | 5.5, 6.0 | 23, 23 | 98% |
| 100 | on | 3.3, 3.5 | 6.4, 6.8 | 36, 37 | 3.3, 3.5 | 5.8, 6.4 | 36, 37 | 86% |
| 500 | off | 10.4, 10.8 | 29, 31 | 283, 283 | 10.1, 10.7 | 26, 27 | 195, 198 | 83% |
| 500 | on | 8.2, 8.8 | 31, 36 | 391, 398 | 7.9, 8.6 | 22, 24 | 266, 263 | 46% |

The windows explain the tail. A bounded patch recomputes about 5 Versions; an open-ended one recomputes 39 to 54 (the window runs to the end of time), and the recompute query joins segments to Contributions, so the cost grows faster than the window. At 500 Contributions the open-ended patches cost 195 to 266 ms at p50 and about 520 to 540 ms at p95. Ranked merge adds no measurable cost and changes fewer Versions, because lower-Authority writes lose.

Each write also touches the Episode, Contribution, Version and Citation tables and takes the exclusion-constraint and FK checks. On average a 500-Contribution write closed 2 to 5 Versions and opened 2 to 5.

### 2. Erasure (one transaction, scratch server with fsync off)

| Target | Cited by Versions | Strategy | Wall time | Replay steps | Version rows deleted | Version rows inserted | Semantic: gone, values rewritten, recorded-to rewritten, new | Oracle |
|---|---|---|---|---|---|---|---|---|
| mid | 2,086 | full | 35.3 s | 17,820 | 64,111 | 61,904 | 3,441, 382, 540, 1,234 | 0 of 3,000 |
| mid | 2,086 | from_earliest | 24.3 s | 8,898 | 39,863 | 37,656 | same | 0 of 3,000 |
| early | 2,804 | full | 34.2 s | 17,376 | 62,306 | 59,850 | 4,774, 500, 213, 2,318 | 0 of 3,000 |
| early | 2,804 | from_earliest | 32.3 s | 14,761 | 58,092 | 55,636 | same | 0 of 3,000 |
| late | 1,305 | full | 32.9 s | 17,196 | 59,664 | 57,783 | 2,234, 120, 779, 353 | 0 of 3,000 |
| late | 1,305 | from_earliest | 9.0 s | 2,577 | 13,130 | 11,249 | same | 0 of 3,000 |

Other counts for the middle run: 500 Contributions and 2,086 Citations deleted directly, and the Tenant's Citations fell from 515,559 to 507,674. The affected Nodes held 64,111 Version rows and 36,061 Contributions before. That is 128 Version rows and 72 Contributions per Node, and about 36 replay steps per Node. Replay costs about 2.0 ms per step (35.2 s for 17,820 steps). Locking took 8 ms, and the deletes of the Episode, Contributions and Citations took 62 ms.

The physical churn is far above the semantic change. The middle `full` run deleted and re-inserted about 126,000 Version rows to change about 5,600 (gone, rewritten, new). That ratio, about 22 to 1, is a cost of truncate-and-replay and not of ADR-0007. The strategy rewrites every row it recomputes, whether or not the row changed.

Concurrent runs (middle target, 8 reader threads):

| Run | Reader p50 control | Reader p50 during Erasure | Reader p95 control | Reader p95 during Erasure | Reader max during | Reader throughput control, during | Write to an affected Node | Write to an unaffected Node, same Tenant |
|---|---|---|---|---|---|---|---|---|
| full | 4.8 ms | 4.2 ms | 6.5 ms | 5.5 ms | 17 ms | 1,625, 1,872 reads/s | waited 33.2 s (until the Erasure committed) | 8.7 ms |
| from_earliest | 4.6 ms | 4.2 ms | 6.3 ms | 5.7 ms | 26 ms | 1,687, 1,840 reads/s | waited 22.7 s (until the Erasure committed) | 7.6 ms |

Reads on affected Nodes and on other Nodes were indistinguishable. The Erasure backend held 500 advisory locks, about 13 relation row-exclusive locks and one transaction id lock. The monitor never saw a read wait on a lock. The only backend seen waiting was the write to an affected Node, waiting on an advisory lock. The reader throughput numbers are bound by the Python client, so compare them with each other and not with a server limit.

### 3. As-of read latency (ms)

| Database | Point read p50, p95, p99 (1 connection) | Batch of 50 Nodes p50, p95 (1 connection) | 8 threads: p50, p95, reads/s |
|---|---|---|---|
| versions only, round a | 0.37, 0.44, 0.53 | 1.05, 1.77 | 4.21, 5.86, 1,842 |
| full schema, round a | 0.37, 0.47, 0.56 | 1.07, 1.75 | 4.55, 6.31, 1,702 |
| versions only, round b | 0.36, 0.45, 0.57 | 1.06, 1.75 | 4.28, 6.04, 1,803 |
| full schema, round b | 0.36, 0.44, 0.51 | 1.06, 1.79 | 4.74, 6.65, 1,639 |
| after a committed Erasure, before vacuum | 0.55, 0.68, 1.07 | 1.56, 2.84 | 6.60, 10.17, 1,226 |
| after a committed Erasure, after vacuum | 0.45, 0.59, 0.83 | 1.31, 2.24 | 4.56, 6.65, 1,679 |

On one connection the Contribution tables make no difference to a read (the read path never touches them). With 8 threads the full schema was 6 to 9% slower in both rounds. I did not find the cause. The extra 2.2 GB of tables competing for the same cache is the likely one, but I did not test it. After an Erasure, reads were about 50% slower until vacuum and about 20% slower afterwards, because the Erasure rewrote tens of thousands of Version rows and left dead rows and index bloat.

## What this means for the ADRs

- ADR-0007 (Versions are computed from Contributions, Erasure recomputes history): confirms. Write cost is small and Erasure cost is bounded by history length. It names "Write and Erasure cost under recompute is unmeasured" as a consequence, and this prototype is the measurement. The consequence can now say: about 1 to 11 ms p50 per patch write by Node size, one open-ended patch costing 200 to 540 ms at 500 Contributions, and 9 to 35 s for 500 Nodes of 36 steps each.
- ADR-0003 (Erasure is one deterministic transaction): confirms, with one cost. One transaction that recomputes 500 Nodes ran 24 to 35 seconds here and held a lock on each Node for all of it. Writers to those Nodes waited for the Erasure to commit.
- ADR-0009 (Store functions run as the invoker, Erasure is the definer): confirms. The write path ran under RLS as a non-owner with only `UPDATE (recorded_to)` on Versions and no `DELETE`, and Erasure ran as the definer, with the guard trigger passing the definer through.
- ADR-0004 and ADR-0008 (scalar time columns, exclusion constraints): confirms. Exclusion constraints on `tstzrange(...)` expressions held through about 126,000 Version row deletes and inserts per Erasure with no violations.
- Not covered by any ADR: a per-Node serialisation lock between writes and Erasure. I put one in both paths (an advisory lock on the Node). I reasoned that without it a write computed before an Erasure commits could write a Version carrying the erased value, but I did not reproduce that race. The ADRs should say what serialises writes against Erasure.

## Recommendation

Keep ADR-0007 as written and add the measured costs to its consequences. Before the schema is fixed:

1. Record the per-Node lock between writes and Erasure in ADR-0003 or ADR-0007, and tell consumers that an Erasure of a widely cited Episode blocks writes to every affected Node until it commits.
2. Do not treat 35 seconds as the cost of Erasure. It is the cost of truncate-and-replay, which rewrites about 22 times more rows than change. A replay that is limited to the affected Valid-time range, or that writes only the rows that differ, should cut both the time and the bloat. I did not build either. If Erasure of a widely cited Episode has to be interactive, measure one before changing the ADR.
3. If Erasure is common, replay only from the earliest affected recorded time (`from_earliest`). It saved 73% when the Episode was late in the history, 31% in the middle and 6% when it was early.
4. Plan a vacuum after Erasure. It leaves heavy dead rows in the Version table and its indexes.
5. Bound or special-case open-ended patches. Recomputing from the start of the interval to the end of time was the only write cost that hurt. This is a property of the window recompute used here, not of the ADR.

## Caveats

- Other prototypes ran at the same time, including two containers at about 100% CPU and a machine load average between 5 and 36. The disk was shared and busy: an 8 KB synchronous write took about 55 ms in the container. Durable-commit latency could not be measured here. In two durable rounds the mean WAL fsync took 94 and 164 ms and every p50 and p95 was dominated by it, so I set `fsync = off` on the scratch server for every number in this document. Each durable commit on the real device adds one WAL flush that these numbers do not include. An earlier durable run, made before the disk got busy, was within about 1.5 ms at p50 of the fsync-off numbers (for example 11.9 against 10.4 ms at 500 Contributions, ranked off); its raw file was not kept. A third round with `synchronous_commit = off` matched the fsync-off round.
- Each Erasure configuration ran once. The write benchmark ran twice and the rounds agree within a few percent, apart from the 99th percentile. The early and late targets are in other Tenants (9 and 10) of similar size to Tenant 1.
- The read thread numbers come from a Python client that tops out near 1,800 reads per second, so use them to compare databases and not as server capacity. The single-connection numbers include one local round trip.
- Simplifications. The merge rule comes from the Node's Kind and never changes, so Erasure does not replay under a rule recorded on each Version (ADR-0007). Statements (end of validity, Retraction) are null-valued Contributions, so Erasure of a Retraction's Episode brings the fact back through the same replay and I did not test that case separately. A Citation goes to every overlapping Contribution that states the winning value. Precision is only `unknown` or `instant`.
- The data are synthetic. Valid-time lengths, history sizes, the 4% open-ended share and the 500-Node, 45-day target are my choices. Real graphs may have longer histories per Node (cost per Erasure grows linearly with steps) or more overlap.
- Writes ran one connection at a time. I did not measure many writers on the same Node or contention on the Tenant-wide indexes.
- `max_locks_per_transaction` was raised to 1024 for the 500 advisory locks of one Erasure. Larger Erasures need a larger lock table or a different lock design.
- The first load stage hit a table-ownership bug in the loader and was completed by `load_more.sh`; neither affects the data used in the runs.

The code is throwaway and lives in `prototypes/recompute-cost/` on this branch. Raw per-write timings are not committed. Summaries are in `prototypes/recompute-cost/results/`.
