"""PROTOTYPE, wipe me. Generate and load the synthetic dataset into one server.

usage: python gen.py PORT USER [--meta path]
Both servers get byte-identical data because every random draw is seeded.
"""
import sys, time, json, hashlib, struct
import numpy as np
import psycopg
from common import *

port, user = int(sys.argv[1]), sys.argv[2]
meta_path = sys.argv[sys.argv.index("--meta") + 1] if "--meta" in sys.argv else None

conn = psycopg.connect(host="127.0.0.1", port=port, user=user, password="proto", dbname="gnomon_proto",
                       options="-c timezone=UTC", autocommit=True)
cur = conn.cursor()
subs_all, sub_words = build_centers()
cdf = zipf_cdf()
HDR = b"PGCOPY\n\xff\r\n\0" + struct.pack(">ii", 0, 0)
TRL = struct.pack(">h", -1)

INF = np.iinfo(np.int64).max
NEG = np.iinfo(np.int64).min
D = 86400
T0 = int(np.datetime64("2024-01-01", "s").astype(np.int64))
TMAX = int(np.datetime64("2026-11-01", "s").astype(np.int64))
VMIN = int(np.datetime64("2020-01-01", "s").astype(np.int64))
VEND_MAX = int(np.datetime64("2026-10-01", "s").astype(np.int64))

EMB = np.dtype([("nf", ">i2"), ("l1", ">i4"), ("tid", "V16"), ("l2", ">i4"), ("sc", "S8"), ("l3", ">i4"), ("h", "V16"),
                ("l4", ">i4"), ("dim", ">i2"), ("un", ">i2"), ("v", ">f4", (DIM,))])
NAME = np.dtype([("nf", ">i2"), ("l1", ">i4"), ("id", ">i8"), ("l2", ">i4"), ("tid", "V16"), ("l3", ">i4"), ("sc", "S8"),
                 ("l4", ">i4"), ("ent", ">i8"), ("l5", ">i4"), ("alias", "u1"), ("l6", ">i4"), ("rf", ">i8"),
                 ("l7", ">i4"), ("rt", ">i8"), ("l8", ">i4"), ("dim", ">i2"), ("un", ">i2"), ("v", ">f4", (DIM,))])
PG_EPOCH = 946684800


def fmt(arr):
    """Seconds since the Unix epoch to timestamptz text, with the INF and NEG sentinels."""
    a = np.asarray(arr, dtype=np.int64)
    pos, neg = a == INF, a == NEG
    s = np.datetime_as_string(np.where(pos | neg, 0, a).astype("datetime64[s]"), unit="s")
    s = np.where(pos, "infinity", np.where(neg, "-infinity", s))
    return s.tolist()


def pg_us(a):
    a = np.asarray(a, dtype=np.int64)
    us = (a - PG_EPOCH) * 1_000_000
    return np.where(a == INF, np.iinfo(np.int64).max, us)


def copy_binary(sql, rec_iter):
    with cur.copy(sql) as cp:
        cp.write(HDR)
        for b in rec_iter:
            cp.write(b)
        cp.write(TRL)


def bodies(rng, sub_t, text_ids, chunk=50000):
    out = []
    for s in range(0, len(sub_t), chunk):
        st = sub_t[s:s + chunk]
        n = len(st)
        L = rng.integers(8, 25, n)
        tw = sub_words[st[:, None], rng.integers(0, WORDS_PER_SUB, (n, 24))]
        gw = np.searchsorted(cdf, rng.random((n, 24)))
        widx = np.where(rng.random((n, 24)) < 0.55, tw, gw)
        words = VOCAB_STR[widx]
        for i in range(n):
            out.append((" ".join(words[i, :L[i]]) + f" u{text_ids[s + i]}", int(L[i]) + 1))
    return out


ver_lines, ep_lines, cit_lines = [], [], []
counters = dict(ver=1, ent=1, ep=1, name=1, text=1)
meta = []
t_start = time.time()
import os
scopes = plan()[:int(os.environ.get("LIMIT", "100000"))]
tot = 0
for sc in scopes:
    N = sc["n"]
    rng = np.random.default_rng(1000 + sc["id"])
    tenant_b = uuid_b = tenant_uuid(sc["tenant_idx"]).bytes
    scope_s = sc["scope"]
    tenant_s = sc["tenant"]
    sc_subs = np.array(sc["subs"])

    # entities and chains of texts
    lens = rng.choice([1, 2, 3], size=int(N / 1.2) + 50, p=[0.6, 0.3, 0.1])
    cs = np.cumsum(lens)
    E = int(np.searchsorted(cs, N)) + 1
    lens = lens[:E].copy()
    lens[-1] -= cs[E - 1] - N
    starts = np.cumsum(lens) - lens
    ent_of = np.repeat(np.arange(E), lens)
    pos = np.arange(N) - starts[ent_of]
    last = pos == (lens[ent_of] - 1)
    kind = rng.choice(np.array(["n", "e", "s", "c"]), size=E, p=[0.4, 0.5, 0.07, 0.03])
    kind[0] = "n"
    ent_id = counters["ent"] + np.arange(E)
    counters["ent"] += E
    nodes = ent_id[kind == "n"]
    sub_e = sc_subs[rng.integers(0, len(sc_subs), E)]
    src = nodes[(len(nodes) * rng.random(E) ** 2.5).astype(int)]
    dst = nodes[rng.integers(0, len(nodes), E)]
    anchor = np.where(kind == "n", ent_id, np.where(kind == "e", src, nodes[rng.integers(0, len(nodes), E)]))
    is_e = kind == "e"

    # texts
    sub_t = sub_e[ent_of]
    text_ids = counters["text"] + np.arange(N)
    counters["text"] += N
    bl = bodies(rng, sub_t, text_ids)
    body = [b for b, _ in bl]
    dl = np.array([d for _, d in bl])
    hashes = [hashlib.md5(b.encode()).digest() for b in body]

    # times
    start0 = T0 + rng.integers(0, 700 * D, E)
    steps = rng.integers(20 * D, 150 * D, (E, 2))
    cum = np.stack([np.zeros(E, dtype=np.int64), steps[:, 0], steps[:, 0] + steps[:, 1]], axis=1)
    rf = start0[ent_of] + cum[ent_of, pos]
    nxt = np.empty(N, dtype=np.int64)
    nxt[:-1] = rf[1:]
    rt = np.where(last, INF, nxt)
    split = rng.random(N) < 0.3
    mid = rf + np.where(last, 30 * D, (np.where(last, rf, nxt) - rf) // 2)
    d1 = VMIN + rng.integers(0, 2000 * D, N)
    d2 = VMIN + rng.integers(0, 2000 * D, N)
    vf1 = np.where(rng.random(N) < 0.5, NEG, d1)
    vf2 = np.where(vf1 == NEG, d2, vf1)
    vt2 = np.where(rng.random(N) < 0.5, INF, np.minimum(vf2 + rng.integers(30 * D, 900 * D, N), VEND_MAX))
    ns = int(split.sum())
    ids1 = counters["ver"] + np.arange(N)
    ids2 = counters["ver"] + N + np.arange(ns)
    counters["ver"] += N + ns
    sidx = np.nonzero(split)[0]

    def lines(idx, ids, vfrom, vto, rfrom, rto):
        f = fmt
        vfs, vts, rfs, rts = f(vfrom), f(vto), f(rfrom), f(rto)
        out = []
        for j, i in enumerate(idx):
            e = ent_of[i]
            k = kind[e]
            out.append(f"{ids[j]}\t{tenant_s}\t{scope_s}\t{k}\t{ent_id[e]}\t{anchor[e]}\t"
                       f"{src[e] if k == 'e' else chr(92) + 'N'}\t{dst[e] if k == 'e' else chr(92) + 'N'}\t"
                       f"{body[i]}\t\\\\x{hashes[i].hex()}\t{dl[i]}\t{vfs[j]}\t{vts[j]}\t{rfs[j]}\t{rts[j]}\n")
        return out

    ver_lines += lines(np.arange(N), ids1, vf1, np.full(N, INF), rf, np.where(split, mid, rt))
    ver_lines += lines(sidx, ids2, vf2[sidx], vt2[sidx], mid[sidx], rt[sidx])

    # episodes and citations
    n_ep = max(2, E // 3)
    ep_ids = counters["ep"] + np.arange(n_ep)
    counters["ep"] += n_ep
    ep_rf = fmt(T0 + rng.integers(0, TMAX - T0, n_ep))
    ep_lines += [f"{ep_ids[i]}\t{tenant_s}\t{scope_s}\t{ep_rf[i]}\n" for i in range(n_ep)]
    cnt = np.minimum(1 + rng.poisson(2, E), 8)
    ce = np.repeat(ent_id, cnt)
    cp_ = ep_ids[rng.integers(0, n_ep, len(ce))]
    cit_lines += [f"{tenant_s}\t{scope_s}\t{ce[i]}\t{cp_[i]}\n" for i in range(len(ce))]

    # embeddings, one per distinct text
    tid16 = np.frombuffer(tenant_b, dtype="V16")[0]
    vrng = np.random.default_rng(5000 + sc["id"])

    def emb_batches():
        B = 10000
        for s in range(0, N, B):
            e = min(N, s + B)
            n = e - s
            rec = np.empty(n, dtype=EMB)
            rec["nf"], rec["l1"], rec["l2"], rec["l3"], rec["l4"] = 4, 16, 8, 16, 4 + 4 * DIM
            rec["tid"] = tid16
            rec["sc"] = scope_s.encode()
            rec["h"] = np.frombuffer(b"".join(hashes[s:e]), dtype="V16")
            rec["dim"], rec["un"] = DIM, 0
            rec["v"] = make_vecs(vrng, subs_all, sub_t[s:e])
            yield rec.tobytes()

    copy_binary("COPY gnomon.emb (tenant_id, scope, text_hash, embedding) FROM STDIN (FORMAT BINARY)", emb_batches())

    # Node names and Aliases, each with its own embedding
    nmask = kind == "n"
    n_nodes = int(nmask.sum())
    slots = 1 + rng.poisson(0.8, n_nodes)
    slot_ent = np.repeat(np.nonzero(nmask)[0], slots)
    slot_idx = np.concatenate([np.arange(s) for s in slots])
    r0 = rf[starts[slot_ent]]
    closed = rng.random(len(slot_ent)) < 0.25
    step = rng.integers(20 * D, 150 * D, len(slot_ent))
    rowA_from = r0
    rowA_to = np.where(closed, r0 + step, INF)
    rowB = np.nonzero(closed)[0]
    n_from = np.concatenate([rowA_from, (r0 + step)[rowB]])
    n_to = np.concatenate([rowA_to, np.full(len(rowB), INF)])
    n_ent = np.concatenate([slot_ent, slot_ent[rowB]])
    n_alias = np.concatenate([slot_idx > 0, (slot_idx > 0)[rowB]])
    n_total = len(n_ent)
    n_ids = counters["name"] + np.arange(n_total)
    counters["name"] += n_total
    n_sub = sub_e[n_ent]
    nrng = np.random.default_rng(9000 + sc["id"])

    def name_batches():
        B = 10000
        for s in range(0, n_total, B):
            e = min(n_total, s + B)
            n = e - s
            rec = np.empty(n, dtype=NAME)
            rec["nf"] = 8
            rec["l1"], rec["l2"], rec["l3"], rec["l4"], rec["l5"], rec["l6"], rec["l7"], rec["l8"] = 8, 16, 8, 8, 1, 8, 8, 4 + 4 * DIM
            rec["id"] = n_ids[s:e]
            rec["tid"] = tid16
            rec["sc"] = scope_s.encode()
            rec["ent"] = ent_id[n_ent[s:e]]
            rec["alias"] = n_alias[s:e].astype("u1")
            rec["rf"] = pg_us(n_from[s:e])
            rec["rt"] = pg_us(n_to[s:e])
            rec["dim"], rec["un"] = DIM, 0
            rec["v"] = make_vecs(nrng, subs_all, n_sub[s:e])
            yield rec.tobytes()

    copy_binary("COPY gnomon.name_item (id, tenant_id, scope, entity_id, is_alias, recorded_from, recorded_to, embedding) "
                "FROM STDIN (FORMAT BINARY)", name_batches())

    tot += N
    meta.append(dict(sc, n_versions=N + ns, n_entities=int(E), n_names=int(n_total)))
    print(f"[{time.time() - t_start:7.0f}s] scope {sc['id']:4d} n={N:8d} total={tot:9d}", flush=True)

# shuffled physical order for the narrow tables, as interleaved inserts would give
rng = np.random.default_rng(31)
for name, lines_, cols in (("ver", ver_lines, "(id, tenant_id, scope, kind, entity_id, anchor_id, src_id, dst_id, body, text_hash, doc_len, valid_from, valid_to, recorded_from, recorded_to)"),
                           ("episode", ep_lines, "(id, tenant_id, scope, recorded_from)"),
                           ("citation", cit_lines, "(tenant_id, scope, entity_id, episode_id)")):
    order = rng.permutation(len(lines_))
    with cur.copy(f"COPY gnomon.{name} {cols} FROM STDIN") as cp:
        for s in range(0, len(order), 50000):
            cp.write("".join(lines_[i] for i in order[s:s + 50000]))
    print(f"[{time.time() - t_start:7.0f}s] loaded {name}: {len(lines_)} rows", flush=True)

if meta_path:
    json.dump(meta, open(meta_path, "w"))
print("DONE", time.time() - t_start, flush=True)
