#!/usr/bin/env python3
"""PROTOTYPE, wipe me. Measurements for the recompute-cost prototype.

Every measured call runs as the non-owner role `authenticated` under RLS: one round trip per transaction,
`begin; set local role authenticated; select set_config('request.jwt.claims', ..., true); <call>; commit`.
Needs PGPASSWORD. postgres:17 on 127.0.0.1:55432. Results go to ./results/*.json.

  bench.py writes   <db> <tag>                       write latency by cell
  bench.py erase    <base_db> <label> <strategy> [--concurrent]
  bench.py reads    <db> <tag>                 as-of read latency
"""
import datetime as dt
import json
import os
import random
import sys
import threading
import time

import psycopg
from psycopg import ClientCursor

HOST = dict(host=os.environ.get("BENCH_HOST", "127.0.0.1"), port=int(os.environ.get("BENCH_PORT", "55432")), user="postgres", password=os.environ["PGPASSWORD"])
RESULTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "results")
os.makedirs(RESULTS, exist_ok=True)
UTC = dt.timezone.utc


def connect(db):
    return psycopg.connect(dbname=db, autocommit=True, cursor_factory=ClientCursor, **HOST)


def claims(tenant):
    return json.dumps({"tenant_id": tenant, "role": "authenticated"})


def tx(cur, tenant, body, params=()):
    """One round trip. Returns the rows of the last statement that returned rows (the call)."""
    pre = "set local synchronous_commit = off; " if os.environ.get("BENCH_SYNC") == "off" else ""
    sql = ("begin; " + pre + "set local role authenticated; select set_config('request.jwt.claims', %s, true); " + body + "; commit")
    try:
        cur.execute(sql, (claims(tenant),) + tuple(params))
        rows = None
        while True:
            if cur.description is not None:
                rows = cur.fetchall()
            if not cur.nextset():
                break
        return rows
    except Exception:
        try:
            cur.connection.execute("rollback")
        except Exception:
            pass
        raise


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def summarise(xs):
    return {"n": len(xs), "p50": pct(xs, 50), "p95": pct(xs, 95), "p99": pct(xs, 99), "mean": sum(xs) / len(xs) if xs else None,
            "max": max(xs) if xs else None}


def save(name, obj):
    with open(os.path.join(RESULTS, name), "w") as f:
        json.dump(obj, f, indent=1, default=str)


def admin():
    return connect("postgres")


def clone(src, dst):
    a = admin()
    a.execute(f'drop database if exists "{dst}" with (force)')
    t0 = time.time()
    a.execute(f'create database "{dst}" template "{src}" strategy file_copy')
    a.close()
    return time.time() - t0


def drop(db):
    a = admin()
    a.execute(f'drop database if exists "{db}" with (force)')
    a.close()


# ---------------------------------------------------------------- 1. write latency

def iso(d):
    return d.isoformat()


def rand_patch(rng, now):
    """A patch write: one Episode, one Valid-time interval, two properties."""
    v0 = dt.datetime(2021, 1, 1, tzinfo=UTC) + dt.timedelta(days=rng.random() * 1826)
    ln = min(400, 7 + rng.expovariate(1 / 55))
    shape = rng.random()
    vf, vt = iso(v0), iso(v0 + dt.timedelta(days=ln))
    if shape < 0.04:
        vt = "infinity"
    p0 = rng.randrange(6)
    patch = {f"prop{p0}": f"v{rng.randrange(4)}", f"prop{(p0 + 1 + rng.randrange(5)) % 6}": f"v{rng.randrange(4)}"}
    ref = iso(now - dt.timedelta(days=rng.random() * 400))
    return rng.randrange(4), ref, vf, vt, json.dumps(patch)


def exp_writes(db, warm=20, measured=250, tag="durable"):
    conn = connect(db)
    cur = conn.cursor()
    cur.execute("select cell, idx, tenant_id, node_id from proto.cell_node order by cell, idx")
    cells = {}
    for cell, idx, t, n in cur.fetchall():
        cells.setdefault(cell, []).append((idx, str(t), str(n)))
    rng = random.Random(42)
    now = dt.datetime.now(UTC)
    rows = []
    names = sorted(cells)
    cur.execute("alter system set track_wal_io_timing = on"); cur.execute("select pg_reload_conf()")
    time.sleep(1)
    cur.execute("select wal_sync, wal_sync_time from pg_stat_wal"); w0 = cur.fetchone()
    # interleave the cells so drift in machine load hits every cell alike
    for i in range(warm + measured):
        order = names[:]
        rng.shuffle(order)
        for cell in order:
            idx, tenant, node = cells[cell][i]
            auth, ref, vf, vt, patch = rand_patch(rng, now)
            t0 = time.perf_counter()
            res = tx(cur, tenant, "select gnomon.write_patch(%s::uuid, %s::uuid, %s, %s::timestamptz, %s::timestamptz, %s::timestamptz, %s::jsonb)",
                     (tenant, node, auth, ref, vf, vt, patch))
            ms = (time.perf_counter() - t0) * 1000
            r = res[0][0]
            rows.append({"cell": cell, "i": i, "warm": i < warm, "ms": ms, "closed": r["closed"], "inserted": r["inserted"], "cands": r["cands"], "open_ended": vt == "infinity"})
    cur.execute("select wal_sync, wal_sync_time from pg_stat_wal"); w1 = cur.fetchone()
    wal = {"fsyncs": w1[0] - w0[0], "fsync_ms_total": w1[1] - w0[1], "fsync_ms_mean": (w1[1] - w0[1]) / max(1, w1[0] - w0[0])}
    save(f"writes_raw_{tag}.json", rows)
    out = {}
    for cell in names:
        m = [r for r in rows if r["cell"] == cell and not r["warm"]]
        ch = [r["ms"] for r in m if r["inserted"] > 0 or r["closed"] > 0]
        no = [r["ms"] for r in m if r["inserted"] == 0 and r["closed"] == 0]
        out[cell] = {"all": summarise([r["ms"] for r in m]),
                     "bounded": summarise([r["ms"] for r in m if not r["open_ended"]]), "open_ended": summarise([r["ms"] for r in m if r["open_ended"]]),
                     "bounded_window_versions": sum(r["cands"] for r in m if not r["open_ended"]) / max(1, len([r for r in m if not r["open_ended"]])),
                     "open_window_versions": sum(r["cands"] for r in m if r["open_ended"]) / max(1, len([r for r in m if r["open_ended"]])),
                     "changed": summarise(ch), "noop": summarise(no),
                     "share_changed": len(ch) / len(m), "mean_window_versions": sum(r["cands"] for r in m) / len(m),
                     "mean_closed": sum(r["closed"] for r in m) / len(m), "mean_inserted": sum(r["inserted"] for r in m) / len(m)}
    out["_wal"] = wal
    save(f"writes_{tag}.json", out)
    return out


# ---------------------------------------------------------------- as-of reads (shared by 3 and the Erasure concurrency test)

def sample_nodes(db, per_tenant=300):
    conn = connect(db)
    cur = conn.cursor()
    cur.execute("""select tenant_id, id from (select tenant_id, id, row_number() over (partition by tenant_id order by random()) rn
                     from gnomon.node n where not exists (select 1 from proto.cell_node c where c.node_id = n.id)) x where rn <= %s""", (per_tenant,))
    rows = [(str(t), str(n)) for t, n in cur.fetchall()]
    conn.close()
    return rows


def read_args(rng, now):
    va = dt.datetime(2021, 1, 1, tzinfo=UTC) + dt.timedelta(days=rng.random() * 1826)
    ka = now - dt.timedelta(days=rng.random() * 700)
    return iso(va), iso(ka)


ASOF = ("select node_id, props from gnomon.version where tenant_id = %s::uuid and node_id = %s::uuid "
        "and valid_from <= %s::timestamptz and valid_to > %s::timestamptz and recorded_from <= %s::timestamptz and recorded_to > %s::timestamptz")
ASOF_BATCH = ("select node_id, props from gnomon.version where tenant_id = %s::uuid and node_id = any(%s::uuid[]) "
              "and valid_from <= %s::timestamptz and valid_to > %s::timestamptz and recorded_from <= %s::timestamptz and recorded_to > %s::timestamptz")


def read_once(cur, tenant_nodes, rng, now):
    tenant, node = rng.choice(tenant_nodes)
    va, ka = read_args(rng, now)
    t0 = time.perf_counter()
    tx(cur, tenant, ASOF, (tenant, node, va, va, ka, ka))
    return (time.perf_counter() - t0) * 1000


def exp_reads(db, tag, n_point=8000, n_batch=1500, threads=8, seconds=12):
    nodes = sample_nodes(db)
    conn = connect(db)
    cur = conn.cursor()
    rng = random.Random(7)
    now = dt.datetime.now(UTC)
    for _ in range(1500):
        read_once(cur, nodes, rng, now)
    point = [read_once(cur, nodes, rng, now) for _ in range(n_point)]
    by_tenant = {}
    for t, n in nodes:
        by_tenant.setdefault(t, []).append(n)
    batch = []
    for _ in range(n_batch):
        t = rng.choice(list(by_tenant))
        ns = rng.sample(by_tenant[t], 50)
        va, ka = read_args(rng, now)
        t0 = time.perf_counter()
        tx(cur, t, ASOF_BATCH, (t, ns, va, va, ka, ka))
        batch.append((time.perf_counter() - t0) * 1000)
    # concurrent point reads
    lat, stop = [], time.time() + seconds

    def worker(seed):
        c = connect(db).cursor()
        r = random.Random(seed)
        mine = []
        while time.time() < stop:
            mine.append(read_once(c, nodes, r, now))
        lat.extend(mine)
    ts = [threading.Thread(target=worker, args=(100 + i,)) for i in range(threads)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    out = {"db": db, "tag": tag, "point_1conn": summarise(point), "batch50_1conn": summarise(batch),
           "point_concurrent": dict(summarise(lat), threads=threads, qps=len(lat) / seconds)}
    save(f"reads_{tag}.json", out)
    return out


# ---------------------------------------------------------------- 2. Erasure

SNAP = """create table proto.pre_version as select v.node_id, v.valid_from, v.valid_to, v.recorded_from, v.recorded_to, v.props
            from gnomon.version v where v.node_id in (select node_id from proto.special_node where label = %s)"""
SNAP_C = """create table proto.pre_cite as select v.node_id, v.valid_from, v.valid_to, v.recorded_from, c.episode_id, c.property
              from gnomon.citation c join gnomon.version v on v.id = c.version_id
             where v.node_id in (select node_id from proto.special_node where label = %s)"""

DIFF = """
with post as (select node_id, valid_from, valid_to, recorded_from, recorded_to, props from gnomon.version
               where node_id in (select node_id from proto.special_node where label = %(l)s))
select
  (select count(*) from proto.pre_version) pre_rows,
  (select count(*) from post) post_rows,
  (select count(*) from proto.pre_version p where not exists (select 1 from post q where (q.node_id, q.valid_from, q.valid_to, q.recorded_from) = (p.node_id, p.valid_from, p.valid_to, p.recorded_from))) rows_gone,
  (select count(*) from proto.pre_version p join post q on (q.node_id, q.valid_from, q.valid_to, q.recorded_from) = (p.node_id, p.valid_from, p.valid_to, p.recorded_from)
    where q.props is distinct from p.props) rows_values_rewritten,
  (select count(*) from proto.pre_version p join post q on (q.node_id, q.valid_from, q.valid_to, q.recorded_from) = (p.node_id, p.valid_from, p.valid_to, p.recorded_from)
    where q.props = p.props and q.recorded_to is distinct from p.recorded_to) rows_recorded_to_rewritten,
  (select count(*) from post q where not exists (select 1 from proto.pre_version p where (q.node_id, q.valid_from, q.valid_to, q.recorded_from) = (p.node_id, p.valid_from, p.valid_to, p.recorded_from))) rows_new,
  (select count(*) from proto.pre_cite) pre_cites,
  (select count(*) from gnomon.citation c join gnomon.version v on v.id = c.version_id where v.node_id in (select node_id from proto.special_node where label = %(l)s)) post_cites
"""

ORACLE = """
with pts as (
  select n.node_id, k.ranked_merge rk, timestamptz '2021-01-01' + random() * interval '1826 days' vat, now() - random() * interval '720 days' kat
    from proto.special_node n join gnomon.node nn on nn.id = n.node_id join gnomon.kind k on k.tenant_id = nn.tenant_id and k.name = nn.kind
    cross join generate_series(1, 6) where n.label = %(l)s
), expect as (
  select p.*, (select jsonb_object_agg(w.property, w.value) from (
       select distinct on (c.property) c.property, c.value from gnomon.contribution c join gnomon.episode e on e.id = c.episode_id
        where c.node_id = p.node_id and c.recorded_at <= p.kat and c.valid_from <= p.vat and c.valid_to > p.vat
        order by c.property, case when p.rk then e.authority else 0 end desc, e.reference_time desc, c.recorded_at desc, c.id desc) w
      where w.value <> 'null'::jsonb) exp from pts p
)
select count(*) samples, count(*) filter (where exp is distinct from (select v.props from gnomon.version v where v.node_id = x.node_id
        and v.valid_from <= x.vat and v.valid_to > x.vat and v.recorded_from <= x.kat and v.recorded_to > x.kat)) disagree from expect x
"""


def counts(cur, tenant):
    cur.execute("""select (select count(*) from gnomon.version where tenant_id = %(t)s), (select count(*) from gnomon.citation where tenant_id = %(t)s),
                          (select count(*) from gnomon.contribution where tenant_id = %(t)s), (select count(*) from gnomon.episode where tenant_id = %(t)s)""", {"t": tenant})
    v, c, k, e = cur.fetchone()
    return {"versions": v, "citations": c, "contributions": k, "episodes": e}


def exp_erase(base, label, strategy, concurrent=False, keep=False):
    run_db = f"gnomon_run_{label}_{strategy}{'_c' if concurrent else ''}"
    clone_s = clone(base, run_db)
    conn = connect(run_db)
    cur = conn.cursor()
    cur.execute("select episode_id, tenant_id from proto.special where label = %s", (label,))
    ep, tenant = [str(x) for x in cur.fetchone()]
    cur.execute("select node_id from proto.special_node where label = %s", (label,))
    affected = [str(r[0]) for r in cur.fetchall()]
    cur.execute("select count(distinct version_id), count(*) from gnomon.citation where episode_id = %s", (ep,))
    cited_versions, cited_rows = cur.fetchone()
    cur.execute("select count(distinct v.node_id) from gnomon.citation c join gnomon.version v on v.id = c.version_id where c.episode_id = %s", (ep,))
    cited_nodes = cur.fetchone()[0]
    cur.execute("select count(*) from gnomon.version where node_id in (select node_id from proto.special_node where label = %s)", (label,))
    affected_versions = cur.fetchone()[0]
    cur.execute("select count(*) from gnomon.contribution where node_id in (select node_id from proto.special_node where label = %s)", (label,))
    affected_contribs = cur.fetchone()[0]
    cur.execute("drop table if exists proto.pre_version"); cur.execute("drop table if exists proto.pre_cite")
    # snapshot as owner
    cur.execute(f"set role gnomon_owner; {SNAP.replace('%s', repr(label))}; {SNAP_C.replace('%s', repr(label))}; reset role")
    before = counts(cur, tenant)
    cur.execute("vacuum (analyze) gnomon.version, gnomon.citation, gnomon.contribution")

    monitor = {"samples": []}
    readers = {"lat": [], "stop": False, "phase": "pre"}
    threads = []
    erase_pid = {}
    if concurrent:
        sample = sample_nodes(run_db, 200)
        same_tenant = [(t, n) for t, n in sample if t == tenant]
        aff_pairs = [(tenant, n) for n in affected]
        rnow = dt.datetime.now(UTC)

        def reader(seed):
            c = connect(run_db).cursor()
            r = random.Random(seed)
            while not readers["stop"]:
                pool = aff_pairs if r.random() < 0.5 else sample
                tenant_, node_ = r.choice(pool)
                va, ka = read_args(r, rnow)
                t0 = time.perf_counter()
                tx(c, tenant_, ASOF, (tenant_, node_, va, va, ka, ka))
                readers["lat"].append((time.time(), (time.perf_counter() - t0) * 1000, readers["phase"], pool is aff_pairs))
        for i in range(8):
            th = threading.Thread(target=reader, args=(900 + i,)); th.start(); threads.append(th)

        def monitor_loop():
            m = connect(run_db).cursor()
            while not readers["stop"]:
                if "pid" in erase_pid:
                    m.execute("""select l.locktype, l.mode, l.granted, count(*) from pg_locks l where l.pid = %s group by 1, 2, 3""", (erase_pid["pid"],))
                    mine = [list(r) for r in m.fetchall()]
                    m.execute("""select count(*) filter (where not granted), count(*) from pg_locks where pid <> %s and pid <> pg_backend_pid()""", (erase_pid["pid"],))
                    other = m.fetchone()
                    m.execute("""select wait_event_type, wait_event, count(*) from pg_stat_activity where datname = current_database() and pid <> pg_backend_pid() and state = 'active' group by 1, 2""")
                    waits = [list(r) for r in m.fetchall()]
                    monitor["samples"].append({"t": time.time(), "erase_locks": mine, "other_ungranted_total": list(other), "active_waits": waits})
                time.sleep(0.25)
        mt = threading.Thread(target=monitor_loop); mt.start(); threads.append(mt)
        time.sleep(6)  # control period: readers only
        readers["phase"] = "erasing"

    # the Erasure, as the invoker role calling the definer function
    ec = connect(run_db)
    ecur = ec.cursor()
    ecur.execute("select pg_backend_pid()")
    erase_pid["pid"] = ecur.fetchone()[0]
    writer = {}
    if concurrent:
        def writer_thread():
            time.sleep(3)
            wc = connect(run_db).cursor()
            r = random.Random(5)
            for kind, node in (("affected", affected[0]), ("unaffected", random.Random(1).choice([n for t, n in same_tenant if n not in affected]))):
                auth, ref, vf, vt, patch = rand_patch(r, dt.datetime.now(UTC))
                t0 = time.perf_counter()
                tx(wc, tenant, "select gnomon.write_patch(%s::uuid, %s::uuid, %s, %s::timestamptz, %s::timestamptz, %s::timestamptz, %s::jsonb)",
                   (tenant, node, auth, ref, vf, vt, patch))
                writer[kind] = {"ms": (time.perf_counter() - t0) * 1000, "finished_at": time.time()}
        wt = threading.Thread(target=writer_thread); wt.start(); threads.append(wt)
    t_start = time.time()
    t0 = time.perf_counter()
    res = tx(ecur, tenant, "select gnomon.erase_episode(%s::uuid, %s::uuid, %s)", (tenant, ep, strategy))
    wall_ms = (time.perf_counter() - t0) * 1000
    t_end = time.time()
    metrics = res[0][0]
    if concurrent:
        readers["phase"] = "post"
        time.sleep(4)
        readers["stop"] = True
        for th in threads:
            th.join()
    after = counts(cur, tenant)
    diff_sql = DIFF.replace("%(l)s", repr(label))
    cur.execute(f"set role gnomon_owner; {ORACLE.replace('%(l)s', repr(label))}")
    # the oracle is the last statement; fetch its rows
    while cur.description is None and cur.nextset():
        pass
    oracle = cur.fetchone()
    cur.execute("reset role")
    cur.execute(diff_sql)
    d = dict(zip([c.name for c in cur.description], cur.fetchone()))
    cur.execute("select count(*) from gnomon.citation where episode_id = %s", (ep,))
    left_cites = cur.fetchone()[0]
    cur.execute("select count(*) from gnomon.contribution where episode_id = %s", (ep,))
    left_contribs = cur.fetchone()[0]
    out = {"label": label, "strategy": strategy, "concurrent": concurrent, "clone_s": clone_s,
           "episode_cited_by_versions": cited_versions, "citation_rows": cited_rows, "cited_nodes": cited_nodes,
           "affected_nodes": len(affected), "affected_node_versions_before": affected_versions, "affected_node_contributions_before": affected_contribs,
           "wall_ms": wall_ms, "function_metrics": metrics, "tenant_counts_before": before, "tenant_counts_after": after,
           "semantic_diff": d, "oracle_samples": oracle[0], "oracle_disagree": oracle[1],
           "citations_of_erased_left": left_cites, "contributions_of_erased_left": left_contribs}
    if concurrent:
        lat = readers["lat"]
        def phase(p, aff=None):
            xs = [l[1] for l in lat if l[2] == p and (aff is None or l[3] == aff)]
            return summarise(xs) if xs else None
        out["reads"] = {"control": phase("pre"), "during_erasure": phase("erasing"), "after": phase("post"),
                        "during_affected_nodes": phase("erasing", True), "during_other_nodes": phase("erasing", False),
                        "control_affected_nodes": phase("pre", True), "control_other_nodes": phase("pre", False)}
        out["reads"]["control_qps"] = len([l for l in lat if l[2] == "pre"]) / 6.0
        out["reads"]["erasing_qps"] = len([l for l in lat if l[2] == "erasing"]) / (wall_ms / 1000.0)
        out["writers"] = {k: dict(v, erasure_ended_at_offset_s=t_end - t_start, started_offset_s=3) for k, v in writer.items()}
        # collapse the lock samples: max count per (locktype, mode, granted) held by the Erasure backend
        agg = {}
        ungranted_max = 0
        for s in monitor["samples"]:
            for lt, mode, gr, n in s["erase_locks"]:
                key = f"{lt}/{mode}/{'granted' if gr else 'WAITING'}"
                agg[key] = max(agg.get(key, 0), n)
            ungranted_max = max(ungranted_max, s["other_ungranted_total"][0])
            for wt_, we, n in s["active_waits"]:
                if wt_ == "Lock":
                    agg[f"others waiting on {we}"] = max(agg.get(f"others waiting on {we}", 0), n)
        out["lock_summary_max_per_sample"] = agg
        out["lock_samples"] = len(monitor["samples"])
        out["max_ungranted_locks_seen_from_other_backends"] = ungranted_max
    save(f"erase_{label}_{strategy}{'_concurrent' if concurrent else ''}.json", out)
    ec.close()
    conn.close()
    if not keep:
        drop(run_db)
    return out


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "writes":
        print(json.dumps(exp_writes(sys.argv[2], tag=sys.argv[3]), indent=1, default=str))
    elif cmd == "erase":
        print(json.dumps(exp_erase(sys.argv[2], sys.argv[3], sys.argv[4], concurrent="--concurrent" in sys.argv), indent=1, default=str))
    elif cmd == "reads":
        print(json.dumps(exp_reads(sys.argv[2], sys.argv[3]), indent=1, default=str))
    else:
        sys.exit(__doc__)
