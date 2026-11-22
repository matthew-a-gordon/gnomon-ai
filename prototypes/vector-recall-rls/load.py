# PROTOTYPE, wipe me. Usage: PGPASSWORD=... python load.py <port>
# Creates the scratch schema and loads ~1M clustered 1536-d vectors by binary COPY, interleaved across Tenants.
import sys, time, uuid
import numpy as np, psycopg
from multiprocessing import Pool
from common import *

BLOCK = 20_000
W = world()
SIZES = W[2]
NT = len(SIZES)
ORDER = np.random.default_rng(SEED + 2).permutation(np.repeat(np.arange(NT), SIZES))  # interleaved heap order
TBYTES = np.array([uuid.UUID(tenant_uuid(t)).bytes for t in range(NT)], dtype="S16")

ROW = np.dtype([("nf", ">i2"),
                ("l1", ">i4"), ("tenant", "S16"),
                ("l2", ">i4"), ("episode", "S16"),
                ("l3", ">i4"), ("dim", ">i2"), ("pad", ">i2"), ("v", ">f4", (DIM,))])
HEADER = b"PGCOPY\n\xff\r\n\x00" + (0).to_bytes(4, "big") + (0).to_bytes(4, "big")
TRAILER = (-1).to_bytes(2, "big", signed=True)

def block(args):
    port, b = args
    tenants = ORDER[b * BLOCK:(b + 1) * BLOCK]
    rng = np.random.default_rng(SEED + 1000 + b)
    vec = make_vectors(tenants, rng, W)
    n = len(tenants)
    a = np.empty(n, dtype=ROW)
    a["nf"] = 3; a["l1"] = 16; a["tenant"] = TBYTES[tenants]
    a["l2"] = 16
    a["episode"] = [uuid.UUID(int=int(x)).bytes for x in rng.integers(0, 2**62, n)]
    a["l3"] = 4 + 4 * DIM; a["dim"] = DIM; a["pad"] = 0; a["v"] = vec
    with psycopg.connect(dsn(port), autocommit=True) as c, c.cursor() as cur:
        with cur.copy("copy gnomon.embedding (tenant_id, episode_id, embedding) from stdin (format binary)") as cp:
            cp.write(HEADER); cp.write(a.tobytes()); cp.write(TRAILER)
    return n

if __name__ == "__main__":
    port = int(sys.argv[1]); nb = (len(ORDER) + BLOCK - 1) // BLOCK
    with psycopg.connect(dsn(port), autocommit=True) as c:
        c.execute(open("schema.sql").read())
    t0 = time.time()
    print(f"port {port}: {len(ORDER)} rows, {NT} tenants, {nb} blocks", flush=True)
    done = 0
    with Pool(10) as p:
        for n in p.imap_unordered(block, [(port, b) for b in range(nb)]):
            done += n
            if done % 200_000 < BLOCK: print(f"  {done} rows {time.time()-t0:.0f}s", flush=True)
    with psycopg.connect(dsn(port), autocommit=True) as c:
        c.execute("create index embedding_tenant_idx on gnomon.embedding (tenant_id)")
        c.execute("analyze gnomon.embedding")
        print(c.execute("select count(*), count(distinct tenant_id), pg_size_pretty(pg_table_size('gnomon.embedding')) from gnomon.embedding").fetchone())
    print(f"done in {time.time()-t0:.0f}s")
