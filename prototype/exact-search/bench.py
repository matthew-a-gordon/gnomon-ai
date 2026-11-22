"""PROTOTYPE, wipe me. Benchmarks for the exact-search prototype.

usage: PYTHONPATH=. python bench.py PORT USER TAG test [test ...]
tests: storage t1 t2 t3 t3w t4 t5 t6
env:   SIZES=1k,10k,... limits visible sets; NQ=n overrides the query count.
Every timed call runs inside a transaction as the non-owner role gnomon_app, with claims set by set_config,
through an invoker Store function (or, for t2 plans, the same statement body).
"""
import sys, os, json, time, subprocess, re
import numpy as np
import psycopg
from common import *

port, user, tag = int(sys.argv[1]), sys.argv[2], sys.argv[3]
tests = sys.argv[4:]
CONTAINER = {55437: "gnomon-proto-exact-search-pgvector", 55438: "gnomon-proto-exact-search-supabase"}[port]
conn = psycopg.connect(host="127.0.0.1", port=port, user=user, password="proto", dbname="gnomon_proto",
                       options="-c timezone=UTC", autocommit=True)
meta = json.load(open("results/meta.json"))
subs_all, sub_words = build_centers()
NOW = "2026-11-21T12:00:00+00"
PAST_K = "2025-06-01T00:00:00+00"
PAST_V = "2025-01-01T00:00:00+00"
T0 = meta[0]["tenant"]
T1 = meta[4]["tenant"]
sc = {m["id"]: m for m in meta}
small = min((m for m in meta if m["tenant_idx"] >= 2), key=lambda m: abs(m["n"] - 1000))
VS = {
    "1k": (T0, [sc[1]["scope"]]),
    "10k": (T0, [sc[1]["scope"], sc[2]["scope"]]),
    "50k": (T0, [sc[i]["scope"] for i in (1, 2, 3)]),
    "200k": (T0, [sc[i]["scope"] for i in (1, 2, 3, 4)]),
    "1M": (T1, [sc[5]["scope"]]),
    "tenant1k": (small["tenant"], [small["scope"]]),
}
SIZES = os.environ.get("SIZES", "1k,10k,50k,200k,1M").split(",")
NQ = int(os.environ["NQ"]) if "NQ" in os.environ else None
RES = {}


def nq_for(name, heavy=False):
    if NQ:
        return NQ
    base = {"1k": 60, "tenant1k": 60, "10k": 60, "50k": 40, "200k": 30, "1M": 12}[name]
    return max(8, base // 3) if heavy else base


def queries(name, n, seed=0):
    tenant, scopes = VS[name]
    rng = np.random.default_rng(100 + seed)
    ms = [m for m in meta if m["tenant"] == tenant and m["scope"] in scopes]
    w = np.array([m["n"] for m in ms], dtype=float)
    out = []
    for _ in range(n):
        m = ms[rng.choice(len(ms), p=w / w.sum())]
        s = int(rng.choice(m["subs"]))
        v = make_vecs(rng, subs_all, np.array([s]))[0]
        words = rng.choice(sub_words[s], 3, replace=False)
        out.append(("[" + ",".join(f"{x:.6g}" for x in v) + "]", " ".join(VOCAB_STR[words])))
    return out


def call(sql, params, vs, local=()):
    tenant, scopes = vs
    with conn.transaction():
        c = conn.cursor()
        c.execute("set local role gnomon_app")
        c.execute("select set_config('gnomon.tenant', %s, true), set_config('gnomon.scopes', %s, true)",
                  (tenant, ",".join(scopes)))
        for l in local:
            c.execute(l)
        t = time.perf_counter()
        c.execute(sql, params)
        rows = c.fetchall()
        return (time.perf_counter() - t) * 1000, rows


def pct(xs):
    a = np.array(xs)
    return dict(p50=round(float(np.percentile(a, 50)), 2), p95=round(float(np.percentile(a, 95)), 2),
                mean=round(float(a.mean()), 2), max=round(float(a.max()), 2), n=len(a))


def run_variant(sql_fn, vs, qs, local=(), warm=None):
    """sql_fn(q, text) -> (sql, params). Returns (stats, rows per query)."""
    # warm pass over the whole query set, so re-rank reads of full vectors are cached (steady state for a hot Tenant)
    for i in range(len(qs) if warm is None else min(warm, len(qs))):
        call(*sql_fn(*qs[i]), vs, local)
    ts, rows = [], []
    for q, t in qs:
        ms, r = call(*sql_fn(q, t), vs, local)
        ts.append(ms)
        rows.append(r)
    return pct(ts), rows


def recall(truth, got, k=10):
    return float(np.mean([len({(a, bytes(b)) for a, b, *_ in t[:k]} & {(a, bytes(b)) for a, b, *_ in g[:k]}) / k
                          for t, g in zip(truth, got)]))


def md(rows, cols):
    print("| " + " | ".join(cols) + " |")
    print("|" + "---|" * len(cols))
    for r in rows:
        print("| " + " | ".join(str(r.get(c, "")) for c in cols) + " |")


def save(name):
    json.dump(RES.get(name), open(f"results/{tag}-{name}.json", "w"), indent=1, default=str)


def dbq(sql, args=None):
    return conn.execute(sql, args).fetchall()


# ------------------------------------------------------------------------------------------
def storage():
    rows = dbq("""select c.relname, c.relkind, pg_relation_size(c.oid), pg_total_relation_size(c.oid)
                  from pg_class c where c.relnamespace = 'gnomon'::regnamespace and c.relkind in ('r', 'i') order by 3 desc""")
    out = [dict(rel=r[0], kind=r[1], size_mb=round(r[2] / 2**20, 1), total_mb=round(r[3] / 2**20, 1)) for r in rows]
    counts = {t: dbq(f"select count(*) from gnomon.{t}")[0][0] for t in
              ("emb", "emb_half", "emb_bit", "ver", "tok", "tok_df", "episode", "citation", "name_item", "name_bit")}
    RES["storage"] = dict(rels=out, counts=counts, ver=dbq("select version(), (select extversion from pg_extension where extname='vector')")[0])
    md(out, ["rel", "kind", "size_mb", "total_mb"])
    print(counts, RES["storage"]["ver"])
    save("storage")


def t1():
    """Exact vector speed and recall: full, halfvec, bit, with full-precision re-rank."""
    res = []
    for name in SIZES:
        qs = queries(name, nq_for(name))
        vs = VS[name]
        f = lambda q, t: ("select * from gnomon.vsearch_full(%s::uuid, %s::public.vector, 10)", (vs[0], q))
        st, truth = run_variant(f, vs, qs)
        res.append(dict(size=name, variant="full", rerank="", recall10=1.0, **st))
        print(res[-1], flush=True)
        for kind, label, local in (("half", "half", ()), ("bit", "bit (heap fetch)", ("set local enable_indexonlyscan = off",)),
                                   ("bit", "bit (covering index)", ())):
            if name == "1M" and label != "bit (covering index)":
                continue
            for rr in (200, 400) if name == "1M" else (100, 200, 400):
                g = lambda q, t: (f"select * from gnomon.vsearch_{kind}(%s::uuid, %s::public.vector, {rr}, 10)", (vs[0], q))
                st, got = run_variant(g, vs, qs, local=local)
                res.append(dict(size=name, variant=label, rerank=rr, recall10=round(recall(truth, got), 3), **st))
                print(res[-1], flush=True)
    RES["t1"] = res
    md(res, ["size", "variant", "rerank", "recall10", "p50", "p95", "n"])
    save("t1")


def capture_plans(label, sql, params, vs):
    """Run once with auto_explain on, return the plan log text for the nested statements."""
    conn.execute("load 'auto_explain'")
    conn.execute("set auto_explain.log_min_duration = 0")
    conn.execute("set auto_explain.log_nested_statements = on")
    conn.execute("set auto_explain.log_analyze = on")
    conn.execute("set auto_explain.log_format = 'text'")
    t0 = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(time.time() - 1))
    call(sql, params, vs)
    conn.execute("set auto_explain.log_min_duration = -1")
    time.sleep(0.5)
    out = subprocess.run(["docker", "logs", "--since", t0 + "Z", CONTAINER], capture_output=True, text=True)
    return out.stdout + out.stderr


def t2():
    """Scope policy: index condition or filter, three policy forms."""
    res = []
    for mode in ("any", "func", "inline"):
        conn.execute("select gnomon.proto_set_policy(%s)", (mode,))
        for name in ("1k", "200k"):
            vs = VS[name]
            qs = queries(name, nq_for(name))
            g = lambda q, t: ("select * from gnomon.vsearch_bit(%s::uuid, %s::public.vector, 200, 10)", (vs[0], q))
            st, _ = run_variant(g, vs, qs)
            plan = capture_plans(mode, g(*qs[0])[0], g(*qs[0])[1], vs)
            lines = [l.strip() for l in plan.splitlines() if re.search(r"Index Cond|Filter|Scan|Rows Removed", l)]
            lines = [l for l in lines if "gnomon.emb_bit" in l or "Cond" in l or "Filter" in l or "Removed" in l][:8]
            res.append(dict(policy=mode, visible=name, **st, plan=lines))
            print(res[-1], flush=True)
    conn.execute("select gnomon.proto_set_policy('any')")
    RES["t2"] = res
    md(res, ["policy", "visible", "p50", "p95", "n"])
    save("t2")


T3_NOW = (NOW, NOW)


def t3():
    """Full-text: tsvector scan against token table (BM25), at now."""
    res = []
    for name in SIZES:
        vs = VS[name]
        qs = queries(name, nq_for(name))
        a = lambda q, t: ("select * from gnomon.fts_scan(%s::uuid, %s, %s::timestamptz, %s::timestamptz, 100)", (vs[0], t, NOW, NOW))
        st, ra = run_variant(a, vs, qs)
        res.append(dict(size=name, variant="tsvector scan", **st))
        print(res[-1], flush=True)
        b = lambda q, t: ("select ver_id, scope, anchor_id, rnk from gnomon.c_fts(%s::uuid, %s, %s::timestamptz, %s::timestamptz, 100)", (vs[0], t, NOW, NOW))
        st, rb = run_variant(b, vs, qs)
        ov = float(np.mean([len({x[0] for x in p[:10]} & {x[0] for x in r[:10]}) / 10 for p, r in zip(ra, rb)]))
        res.append(dict(size=name, variant="token table BM25", top10_overlap_with_scan=round(ov, 2), **st))
        print(res[-1], flush=True)
        c = lambda q, t: ("select ver_id, scope, anchor_id, rnk from gnomon.c_fts_late(%s::uuid, %s, %s::timestamptz, %s::timestamptz, 100)", (vs[0], t, NOW, NOW))
        st, rc = run_variant(c, vs, qs)
        ov2 = float(np.mean([len({x[0] for x in p[:10]} & {x[0] for x in r[:10]}) / 10 for p, r in zip(rb, rc)]))
        res.append(dict(size=name, variant="token table BM25, As-of on top 4x only", top10_overlap_with_exact_asof=round(ov2, 2), **st))
        print(res[-1], flush=True)
    RES["t3"] = res
    md(res, ["size", "variant", "p50", "p95", "n"])
    save("t3")


TRIG_ROW_TOK = """create or replace function gnomon.trg_tok_row() returns trigger language plpgsql set search_path = '' as $$
begin insert into gnomon.tok select NEW.tenant_id, NEW.scope, t.lexeme, NEW.id, pg_catalog.array_length(t.positions, 1), NEW.doc_len
        from pg_catalog.unnest(NEW.fts) t; return null; end $$"""
TRIG_ROW_ALL = """create or replace function gnomon.trg_tok_row() returns trigger language plpgsql set search_path = '' as $$
begin
  insert into gnomon.tok select NEW.tenant_id, NEW.scope, t.lexeme, NEW.id, pg_catalog.array_length(t.positions, 1), NEW.doc_len
    from pg_catalog.unnest(NEW.fts) t;
  insert into gnomon.tok_df select NEW.tenant_id, t.lexeme, 1 from pg_catalog.unnest(NEW.fts) t
    on conflict (tenant_id, token) do update set df = gnomon.tok_df.df + 1;
  update gnomon.tenant_stats set n_docs = n_docs + 1, total_len = total_len + NEW.doc_len where tenant_id = NEW.tenant_id;
  return null; end $$"""
TRIG_STMT = """create or replace function gnomon.trg_tok_stmt() returns trigger language plpgsql set search_path = '' as $$
begin
  insert into gnomon.tok select v.tenant_id, v.scope, t.lexeme, v.id, pg_catalog.array_length(t.positions, 1), v.doc_len
    from nt v cross join lateral pg_catalog.unnest(v.fts) t;
  insert into gnomon.tok_df select x.tenant_id, x.token, count(*) from
    (select v.tenant_id, t.lexeme as token from nt v cross join lateral pg_catalog.unnest(v.fts) t) x group by 1, 2
    on conflict (tenant_id, token) do update set df = gnomon.tok_df.df + excluded.df;
  update gnomon.tenant_stats s set n_docs = s.n_docs + a.n, total_len = s.total_len + a.l
    from (select tenant_id, count(*) as n, sum(doc_len) as l from nt group by 1) a where s.tenant_id = a.tenant_id;
  return null; end $$"""


def t3w():
    """Write cost per Version for the token-table trigger variants."""
    rng = np.random.default_rng(5)
    scope = sc[3]["scope"]
    vs = (T0, [scope])
    sub = sc[3]["subs"][0]
    base_id = 900_000_000

    def rows(n, off):
        out = []
        for i in range(n):
            ws = list(VOCAB_STR[sub_words[sub][rng.integers(0, 60, 7)]]) + list(VOCAB_STR[np.searchsorted(zipf_cdf(), rng.random(7))])
            out.append((base_id + off + i, " ".join(ws) + f" ux{off + i}", len(ws) + 1))
        return out

    variants = [("no trigger", None, None), ("row trigger, token table only", TRIG_ROW_TOK, "row"),
                ("row trigger, token table + df + stats", TRIG_ROW_ALL, "row"),
                ("statement trigger (transition table), all three", TRIG_STMT, "stmt")]
    res = []
    for label, fn, mode in variants:
        conn.execute("drop trigger if exists trg_tok on gnomon.ver")
        if fn:
            conn.execute(fn)
            if mode == "row":
                conn.execute("create trigger trg_tok after insert on gnomon.ver for each row execute function gnomon.trg_tok_row()")
            else:
                conn.execute(TRIG_STMT)
                conn.execute("create trigger trg_tok after insert on gnomon.ver referencing new table as nt for each statement execute function gnomon.trg_tok_stmt()")
        for shape, n, per in (("single-row inserts", 1500, 1), ("batch of 1000", 5000, 1000)):
            ts = []
            for rep in range(3):
                data = rows(n, rep * 10000)
                with conn.transaction():
                    c = conn.cursor()
                    c.execute("set local role gnomon_app")
                    c.execute("select set_config('gnomon.tenant', %s, true), set_config('gnomon.scopes', %s, true)", (vs[0], scope))
                    t = time.perf_counter()
                    for s in range(0, n, per):
                        chunk = data[s:s + per]
                        vals = ",".join(
                            f"({r[0]},'{T0}','{scope}','n',{r[0]},{r[0]},null,null,'{r[1]}','\\x00'::bytea,{r[2]},'-infinity','infinity','{NOW}','infinity')"
                            for r in chunk)
                        c.execute("insert into gnomon.ver (id,tenant_id,scope,kind,entity_id,anchor_id,src_id,dst_id,body,text_hash,doc_len,valid_from,valid_to,recorded_from,recorded_to) values " + vals)
                    ts.append((time.perf_counter() - t) / n * 1e6)
                    raise psycopg.Rollback()
            res.append(dict(variant=label, shape=shape, us_per_version_p50=round(float(np.median(ts)), 1), runs=[round(x, 1) for x in ts]))
            print(res[-1], flush=True)
    conn.execute("drop trigger if exists trg_tok on gnomon.ver")
    RES["t3w"] = res
    md(res, ["variant", "shape", "us_per_version_p50"])
    save("t3w")


def truth_asof(vs, q, k, v):
    sql = """select e.scope, e.text_hash from gnomon.emb e where e.tenant_id = %s::uuid
             and exists (select 1 from gnomon.ver x where x.tenant_id = e.tenant_id and x.scope = e.scope and x.text_hash = e.text_hash
               and x.valid_from <= %s::timestamptz and %s::timestamptz < x.valid_to and x.recorded_from <= %s::timestamptz and %s::timestamptz < x.recorded_to)
             order by e.embedding <=> %s::public.vector limit 10"""
    return call(sql, (vs[0], v, v, k, k, q), vs)[1]


def t4():
    """As-of vector search with embeddings shared by text hash, and the current-only name index."""
    res = []
    for name in [x for x in SIZES if x != "1M"]:
        vs = VS[name]
        qs = queries(name, nq_for(name))
        base = lambda q, t: ("select * from gnomon.c_vec_noasof(%s::uuid, %s::public.vector, 200, 100)", (vs[0], q))
        st, _ = run_variant(base, vs, qs)
        res.append(dict(size=name, variant="no As-of conditions (bit, rerank 200)", **st))
        print(res[-1], flush=True)
        for label, k, v, fnn in [(l + sfx, k, v, f) for (l, k, v) in (("As-of now", NOW, NOW), ("As-of past known-at", PAST_K, PAST_V))
                                 for (sfx, f) in ((", hash join", "c_vec"), (", per-Version bit rows", "c_vec2"))
                                 if not (name in ("200k", "1M") and f == "c_vec")]:
            g = lambda q, t: (f"select ver_id, scope, anchor_id, rnk from gnomon.{fnn}(%s::uuid, %s::public.vector, %s::timestamptz, %s::timestamptz, 200, 100)", (vs[0], q, v, k))
            st, got = run_variant(g, vs, qs)
            # recall of the shared-hash bit path against exact full precision with the same As-of conditions
            nrec = min(len(qs), 12)
            rr = []
            for i in range(nrec):
                tr = {(a, bytes(b)) for a, b in truth_asof(vs, qs[i][0], k, v)}
                gh = call("select v.scope, v.text_hash from gnomon.ver v where v.id = any(%s)", ([r[0] for r in got[i][:10]],), vs)[1]
                rr.append(len(tr & {(a, bytes(b)) for a, b in gh}) / 10)
            res.append(dict(size=name, variant=label, recall10=round(float(np.mean(rr)), 3), results=float(np.mean([len(r) for r in got])), **st))
            print(res[-1], flush=True)
    RES["t4"] = res
    md(res, ["size", "variant", "recall10", "p50", "p95", "n"])
    # names and Aliases
    nres = []
    for name in SIZES:
        vs = VS[name]
        qs = queries(name, nq_for(name))
        f = lambda q, t: ("select * from gnomon.name_search_cur_full(%s::uuid, %s::public.vector, 10)", (vs[0], q))
        st, truth = run_variant(f, vs, qs)
        nres.append(dict(size=name, variant="names, current-only partial index, full vectors", recall10=1.0, **st))
        print(nres[-1], flush=True)
        for rr_ in (100, 200):
            g = lambda q, t: (f"select * from gnomon.name_search_cur(%s::uuid, %s::public.vector, {rr_}, 10)", (vs[0], q))
            st, got = run_variant(g, vs, qs)
            rc = float(np.mean([len({x[0] for x in a[:10]} & {x[0] for x in b[:10]}) / 10 for a, b in zip(truth, got)]))
            nres.append(dict(size=name, variant=f"names, current-only partial index, bit + rerank {rr_}", recall10=round(rc, 3), **st))
            print(nres[-1], flush=True)
        h = lambda q, t: ("select * from gnomon.name_search_asof(%s::uuid, %s::public.vector, %s::timestamptz, 200, 10)", (vs[0], q, PAST_K))
        st, _ = run_variant(h, vs, qs)
        nres.append(dict(size=name, variant="names at a past known-at (all rows, bit + rerank 200)", **st))
        print(nres[-1], flush=True)
    RES["t4names"] = nres
    md(nres, ["size", "variant", "recall10", "p50", "p95", "n"])
    save("t4")
    json.dump(nres, open(f"results/{tag}-t4names.json", "w"), indent=1, default=str)


def t5():
    """Fusion with graph signals."""
    res = []
    for name in [s for s in SIZES if s != "1M"]:
        vs = VS[name]
        qs = queries(name, nq_for(name, heavy=True))
        for label, k, v in (("now", NOW, NOW), ("past known-at", PAST_K, PAST_V)):
            for depth in (1, 2):
                g = lambda q, t: ("select * from gnomon.search(%s::uuid, %s::public.vector, %s, %s::timestamptz, %s::timestamptz, %s)", (vs[0], q, t, v, k, depth))
                st, _ = run_variant(g, vs, qs)
                res.append(dict(size=name, asof=label, part=f"search total, depth {depth}", **st))
                print(res[-1], flush=True)
            parts = {
                "c_vec2": lambda q, t: ("select * from gnomon.c_vec2(%s::uuid, %s::public.vector, %s::timestamptz, %s::timestamptz, 200, 100)", (vs[0], q, v, k)),
                "c_fts": lambda q, t: ("select * from gnomon.c_fts(%s::uuid, %s, %s::timestamptz, %s::timestamptz, 100)", (vs[0], t, v, k)),
            }
            for pn, fn in parts.items():
                st, _ = run_variant(fn, vs, qs)
                res.append(dict(size=name, asof=label, part=pn, **st))
                print(res[-1], flush=True)
            # signals and neighbourhood on candidates taken from c_vec
            cand = [[r[0] for r in call(*parts["c_vec2"](q, t), vs)[1]] for q, t in qs]
            seeds = [call(*parts["c_vec2"](q, t), vs)[1][:10] for q, t in qs]
            ts = []
            for ids in cand[3:] + cand[:3]:
                ts.append(call("select * from gnomon.signals(%s::uuid, %s::bigint[], %s::timestamptz, %s::timestamptz)", (vs[0], ids, v, k), vs)[0])
            res.append(dict(size=name, asof=label, part="signals (mentions + connectedness) for 100 candidates", **pct(ts)))
            print(res[-1], flush=True)
            ts = []
            for ids in cand[3:] + cand[:3]:
                ts.append(call("select * from gnomon.signals_roll(%s::uuid, %s::bigint[])", (vs[0], ids), vs)[0])
            res.append(dict(size=name, asof=label, part="signals from the per-Scope rollup (now only)", **pct(ts)))
            print(res[-1], flush=True)
            for depth in (1, 2):
                ts = []
                for sd in seeds[3:] + seeds[:3]:
                    ts.append(call("select * from gnomon.c_nbr(%s::uuid, %s::text[], %s::bigint[], %s, %s::timestamptz, %s::timestamptz, 100)",
                                   (vs[0], [r[1] for r in sd], [r[2] for r in sd], depth, v, k), vs)[0])
                res.append(dict(size=name, asof=label, part=f"neighbourhood depth {depth} from 10 seeds", **pct(ts)))
                print(res[-1], flush=True)
    RES["t5"] = res
    md(res, ["size", "asof", "part", "p50", "p95", "n"])
    save("t5")


def t6():
    """Run after the container is capped. Best variant at 200k: bit + rerank, As-of search, fusion."""
    old = list(SIZES)
    globals()["SIZES"] = ["200k"]
    res = []
    vs = VS["200k"]
    qs = queries("200k", 40)
    for label, fn in (("bit rerank 200 (no As-of)", lambda q, t: ("select * from gnomon.vsearch_bit(%s::uuid, %s::public.vector, 200, 10)", (vs[0], q))),
                      ("full vectors", lambda q, t: ("select * from gnomon.vsearch_full(%s::uuid, %s::public.vector, 10)", (vs[0], q))),
                      ("c_vec As-of now", lambda q, t: ("select * from gnomon.c_vec(%s::uuid, %s::public.vector, %s::timestamptz, %s::timestamptz, 200, 100)", (vs[0], q, NOW, NOW))),
                      ("search total, depth 1", lambda q, t: ("select * from gnomon.search(%s::uuid, %s::public.vector, %s, %s::timestamptz, %s::timestamptz, 1)", (vs[0], q, t, NOW, NOW)))):
        # first pass cold: no warm-up
        st_cold, _ = run_variant(fn, vs, qs[:10], warm=0)
        st, _ = run_variant(fn, vs, qs, warm=3)
        res.append(dict(variant=label, cold_first10_p50=st_cold["p50"], cold_first10_max=st_cold["max"], **st))
        print(res[-1], flush=True)
    sizes = dbq("select current_setting('shared_buffers'), current_setting('work_mem'), current_setting('effective_cache_size')")[0]
    RES["t6"] = dict(settings=sizes, rows=res)
    md(res, ["variant", "cold_first10_p50", "cold_first10_max", "p50", "p95", "n"])
    save("t6")


for t in tests:
    print(f"==== {t} on {tag} ====", flush=True)
    globals()[t]()
