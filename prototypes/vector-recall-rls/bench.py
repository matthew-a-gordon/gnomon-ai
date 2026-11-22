# PROTOTYPE, wipe me. Usage: PGPASSWORD=... python bench.py <port> <label> [group ...]
# Every measured query runs as the ADR-0001/0005 caller: SET LOCAL ROLE authenticated + set_config(claims, true) per transaction.
import sys, json, time
import numpy as np, psycopg
from common import *

import os
PORT, LABEL = int(sys.argv[1]), sys.argv[2]
TABLE = os.environ.get("TABLE", "gnomon.embedding")   # gnomon.embedding_p = hash-partitioned copy
GROUPS = sys.argv[3:] or ["plan", "hnsw", "iter", "tune", "exact", "partial", "policy", "unfiltered"]
W = world()
def _mid():
    sz = layout(); med = np.arange(10, len(sz))
    near2k = med[np.argsort(np.abs(sz[med] - 2000))[:3]].tolist()
    biggest = med[np.argsort(-sz[med])[:3]].tolist()
    return near2k, biggest
_M2K, _MMAX = _mid()
CLASSES = {"L200k": [0], f"Mmax~{int(layout()[_MMAX[0]])//1000}k": _MMAX, "M2k": _M2K, "S1000": [7, 8, 9], "S300": [4, 5, 6], "S100": [1, 2, 3]}
Q_PER_CLASS = 90
K = 10
OUT = open(f"results-{LABEL}.jsonl", "a")
GT_TABLE = "gnomon.embedding"

def queries():
    out = {}
    for mode, label in (("topical", "topical"), ("row", "near")):
        for cls, ts in CLASSES.items():
            rng = np.random.default_rng(SEED + 5000 + ts[0] + (0 if mode == "topical" else 77))
            tl = np.repeat(ts, Q_PER_CLASS // len(ts)) if len(ts) > 1 else np.full(Q_PER_CLASS, ts[0])
            v = make_vectors(tl.astype(np.int64), rng, W, mode)
            out[f"{cls}/{label}"] = [(int(t), x) for t, x in zip(tl, v)]
    return out

def vtxt(x): return "[" + ",".join(f"{f:.6g}" for f in x) + "]"

def connect():
    c = psycopg.connect(dsn(PORT), autocommit=True, prepare_threshold=None)  # no prepared plans: custom plan per query
    return c

def truth(c, qs):
    gt = {}
    for cls, lst in qs.items():
        gt[cls] = []
        for t, x in lst:
            rows = c.execute(f"select id from {GT_TABLE} where tenant_id = %s order by (embedding <=> %s::vector) + 0 limit %s",
                             (tenant_uuid(t), vtxt(x), K)).fetchall()
            gt[cls].append({r[0] for r in rows})
    return gt

def run(c, cls, lst, gt, name, settings="", sql_tpl=None, group="", warm=5, timeout_ms=60000):
    tpl = (sql_tpl or "select id from gnomon.embedding order by embedding <=> %s::vector limit 10").replace("gnomon.embedding", TABLE)
    lat, rec, nrows = [], [], []
    for i, (t, x) in enumerate(lst):
        claims = json.dumps({"tenant_id": tenant_uuid(t), "role": "authenticated"})
        setup = f"begin; set local role authenticated; select set_config('request.jwt.claims', '{claims}', true); set local statement_timeout = {timeout_ms}; {settings}"
        sql = tpl.replace("{tenant}", tenant_uuid(t))
        t0 = time.perf_counter()
        try:
            c.execute(setup)
            ids = {r[0] for r in c.execute(sql, (vtxt(x),)).fetchall()}
            c.execute("commit")
        except psycopg.errors.QueryCanceled:
            c.execute("rollback"); ids = set()
            lat.append(timeout_ms); rec.append(0.0); nrows.append(0); continue
        dt = (time.perf_counter() - t0) * 1000
        if i >= warm:  # first few queries of a config warm the cache and are not counted
            lat.append(dt); rec.append(len(ids & gt[i]) / K); nrows.append(len(ids))
    r = dict(server=LABEL, table=TABLE, group=group, cls=cls, config=name, n=len(lat),
             recall_mean=round(float(np.mean(rec)), 4), recall_min=round(float(np.min(rec)), 2),
             rows_mean=round(float(np.mean(nrows)), 2),
             p50_ms=round(float(np.percentile(lat, 50)), 1), p95_ms=round(float(np.percentile(lat, 95)), 1))
    OUT.write(json.dumps(r) + "\n"); OUT.flush()
    print(f"{LABEL} {group:7s} {cls:14s} {name:46s} recall={r['recall_mean']:.3f} (min {r['recall_min']:.1f}) rows={r['rows_mean']:4.1f} p50={r['p50_ms']:8.1f} p95={r['p95_ms']:8.1f}", flush=True)
    return r

FORCE = "set local enable_sort = off; "   # a Sort node is the only thing the btree plan has that HNSW does not
def hnsw(ef=40, it="off", mst=None, mem=None):
    s = FORCE + f"set local hnsw.ef_search = {ef}; set local hnsw.iterative_scan = {it}; "
    if mst: s += f"set local hnsw.max_scan_tuples = {mst}; "
    if mem: s += f"set local hnsw.scan_mem_multiplier = {mem}; "
    return s

def main():
    qs = queries()
    c = connect()
    print(c.execute("select version()").fetchone()[0][:60], "| pgvector", c.execute("select extversion from pg_extension where extname='vector'").fetchone()[0], flush=True)
    t0 = time.time(); gt = truth(c, qs); print(f"ground truth {time.time()-t0:.0f}s", flush=True)

    if "plan" in GROUPS:   # what the planner does by default, no forcing
        for cls, lst in qs.items():
            t, x = lst[0]
            claims = json.dumps({"tenant_id": tenant_uuid(t)})
            c.execute(f"begin; set local role authenticated; select set_config('request.jwt.claims', '{claims}', true)")
            plan = "\n".join(r[0] for r in c.execute(f"explain select id from {TABLE} order by embedding <=> %s::vector limit 10", (vtxt(x),)).fetchall())
            c.execute("rollback")
            print(f"--- default plan, {cls}\n{plan}", flush=True)
            OUT.write(json.dumps(dict(server=LABEL, group="plan", cls=cls, plan=plan)) + "\n")
        for cls, lst in qs.items():
            run(c, cls, lst, [g for g in gt[cls]], "planner default (no forcing)", group="plan")

    if "hnsw" in GROUPS:
        for ef in (40, 100, 200, 400, 1000):
            for cls, lst in qs.items():
                run(c, cls, lst, gt[cls], f"HNSW ef_search={ef}, iterative off", hnsw(ef), group="hnsw")

    if "iter" in GROUPS:
        for mode in ("relaxed_order", "strict_order"):
            for ef in (40, 100):
                for mst in (20000, 2000000):
                    for cls, lst in qs.items():
                        run(c, cls, lst, gt[cls], f"HNSW {mode} ef={ef} max_scan_tuples={mst}", hnsw(ef, mode, mst), group="iter")

    if "tune" in GROUPS:   # the scan is bounded by work_mem x scan_mem_multiplier, not by max_scan_tuples
        for mode in ("relaxed_order", "strict_order"):
            for mult in (2, 4, 8, 16, 32, 64, 128):
                for cls, lst in qs.items():
                    run(c, cls, lst, gt[cls], f"HNSW {mode} ef=40 mst=2M scan_mem_multiplier={mult}", hnsw(40, mode, 2000000, mult), group="tune", timeout_ms=20000)

    if "exact" in GROUPS:
        sql = "select id from gnomon.embedding order by (embedding <=> %s::vector) + 0 limit 10"
        sqlx = "select id from gnomon.embedding where tenant_id = '{tenant}' order by (embedding <=> %s::vector) + 0 limit 10"
        for cls, lst in qs.items():
            run(c, cls, lst, gt[cls], "exact via tenant btree (RLS qual only)", "", sql, group="exact")
            run(c, cls, lst, gt[cls], "exact via tenant btree (explicit tenant too)", "", sqlx, group="exact")

    if "partial" in GROUPS:
        have = {r[0] for r in c.execute("select indexname from pg_indexes where indexname like 'embedding_hnsw_t%'").fetchall()}
        for cls, ts in CLASSES.items():
            for t in ts:
                name = f"embedding_hnsw_t{t}"
                if name not in have:
                    t0 = time.time()
                    c.execute("set maintenance_work_mem = '4GB'")
                    c.execute(f"create index {name} on gnomon.embedding using hnsw (embedding vector_cosine_ops) where tenant_id = '{tenant_uuid(t)}'")
                    print(f"built {name} in {time.time()-t0:.0f}s", flush=True)
        c.execute("analyze gnomon.embedding")
        sqlp = "select id from gnomon.embedding where tenant_id = '{tenant}' order by embedding <=> %s::vector limit 10"
        for cls, lst in qs.items():
            t, x = lst[0]
            claims = json.dumps({"tenant_id": tenant_uuid(t)})
            c.execute(f"begin; set local role authenticated; select set_config('request.jwt.claims', '{claims}', true)")
            plan = "\n".join(r[0] for r in c.execute("explain " + sqlp.replace("{tenant}", tenant_uuid(t)), (vtxt(x),)).fetchall())
            c.execute("rollback")
            print(f"--- partial plan, {cls}\n{plan}", flush=True)
            run(c, cls, lst, gt[cls], "per-tenant partial HNSW, ef_search=40, iterative off", "set local hnsw.ef_search = 40; set local hnsw.iterative_scan = off; ", sqlp, group="partial")
            run(c, cls, lst, gt[cls], "per-tenant partial HNSW, ef_search=100", "set local hnsw.ef_search = 100; set local hnsw.iterative_scan = off; ", sqlp, group="partial")

    if "unfiltered" in GROUPS:
        unfiltered(c, qs)

    if "policy" in GROUPS:   # the same iterative query, policy written with an initplan around the tenant function
        owner = connect()
        for form, using in (("direct call", "tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope)"),
                            ("initplan (select ...)", "tenant_id = (select gnomon.tenant_id()) and (select gnomon.scope_allowed(scope))" )):
            owner.execute(f"alter policy tenant_isolation on gnomon.embedding using ({using})")
            for cls, lst in qs.items():
                run(c, cls, lst, gt[cls], f"policy {form}: relaxed ef=40 mst=500000", hnsw(40, "relaxed_order", 500000), group="policy")
        owner.execute("alter policy tenant_isolation on gnomon.embedding using (tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope))")

def unfiltered(c, qs):
    """Index quality on its own: no RLS, no tenant filter, recall against the exact global top 10."""
    out = {}
    for cls in ("L200k/near", "L200k/topical"):
        lst = qs[cls][:40]
        gts = []
        for t, x in lst:
            rows = c.execute("select id from gnomon.embedding order by (embedding <=> %s::vector) + 0 limit 10", (vtxt(x),)).fetchall()
            gts.append({r[0] for r in rows})
        for ef in (40, 100, 400, 1000):
            rec, lat = [], []
            for i, (t, x) in enumerate(lst):
                t0 = time.perf_counter()
                c.execute(f"begin; set local enable_sort = off; set local hnsw.ef_search = {ef}")
                ids = {r[0] for r in c.execute("select id from gnomon.embedding order by embedding <=> %s::vector limit 10", (vtxt(x),)).fetchall()}
                c.execute("commit")
                if i >= 3: rec.append(len(ids & gts[i]) / 10); lat.append((time.perf_counter() - t0) * 1000)
            r = dict(server=LABEL, table=TABLE, group="unfiltered", cls=cls, config=f"no RLS, no tenant filter, ef_search={ef}", n=len(rec),
                     recall_mean=round(float(np.mean(rec)), 4), recall_min=round(float(np.min(rec)), 2), rows_mean=10.0,
                     p50_ms=round(float(np.percentile(lat, 50)), 1), p95_ms=round(float(np.percentile(lat, 95)), 1))
            OUT.write(json.dumps(r) + "\n"); OUT.flush()
            print(f"{LABEL} unfilt  {cls:14s} ef={ef:4d} global recall={r['recall_mean']:.3f} (min {r['recall_min']:.1f}) p50={r['p50_ms']:.1f}", flush=True)

main()
