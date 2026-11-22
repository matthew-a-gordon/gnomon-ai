# PROTOTYPE, wipe me. Throwaway code for prototype 1 (vector recall under RLS). Not part of gnomon.
import os, uuid
import numpy as np

DIM = 1536
LATENT = 48
N_GLOBAL_CLUSTERS = 300
SEED = 20261121

# Tenant layout: 0 = large, 1..9 = small (3 each of 100, 300, 1000 rows), 10.. = medium crowd.
LARGE_ROWS = 200_000
SMALL_SIZES = [100, 100, 100, 300, 300, 300, 1000, 1000, 1000]
N_MEDIUM = 2000
MEDIUM_TOTAL = 800_000

def tenant_uuid(t: int) -> str:
    return f"00000000-0000-4000-8000-{t:012d}"

def layout():
    rng = np.random.default_rng(SEED)
    raw = rng.lognormal(0.0, 0.9, N_MEDIUM)
    med = np.maximum(50, np.round(raw / raw.sum() * MEDIUM_TOTAL)).astype(int)
    med[np.argmax(med)] += MEDIUM_TOTAL - med.sum()
    sizes = np.concatenate([[LARGE_ROWS], SMALL_SIZES, med]).astype(int)
    return sizes  # index = tenant number

SUB_ROWS = 15          # rows per "document" sub-cluster inside a Tenant
SUB_SPREAD = 0.6       # how far a sub-cluster sits from its topic centre (latent, per dim)
ROW_NOISE = 0.15       # spread of rows inside a sub-cluster (latent, per dim)
AMBIENT = 0.02         # isotropic noise added in the full 1536 dimensions

def world():
    """Topic centres, projection, per-tenant topics, offsets and sub-clusters. Deterministic."""
    rng = np.random.default_rng(SEED + 1)
    centres = rng.normal(0, 1.0, (N_GLOBAL_CLUSTERS, LATENT)).astype(np.float32)
    q, _ = np.linalg.qr(rng.normal(size=(DIM, LATENT)))
    proj = q.astype(np.float32)  # DIM x LATENT, orthonormal columns
    sizes = layout()
    nt = len(sizes)
    tclusters, subs_c, subs_v = [], [], []
    for t in range(nt):
        k = 40 if t == 0 else int(rng.integers(3, 6))
        tc = rng.choice(N_GLOBAL_CLUSTERS, k, replace=False)
        tclusters.append(tc)
        ns = max(2, int(sizes[t]) // SUB_ROWS)
        subs_c.append(rng.choice(tc, ns))
        subs_v.append(rng.normal(0, SUB_SPREAD, (ns, LATENT)).astype(np.float32))
    offsets = rng.normal(0, 0.25, (nt, LATENT)).astype(np.float32)
    return centres, proj, sizes, tclusters, offsets, subs_c, subs_v

def make_vectors(tenants: np.ndarray, rng, W, mode="row"):
    """One unit-length vector per entry of `tenants`, drawn from that tenant's own mixture.
    mode "row":     a document like the tenant's stored ones (a point in one of its sub-clusters).
    mode "topical": a query about one of the tenant's topics, not tied to one of its documents,
                    so rows of other Tenants in the same topic sit as close or closer."""
    centres, proj, sizes, tclusters, offsets, subs_c, subs_v = W
    n = len(tenants)
    base = np.empty((n, LATENT), dtype=np.float32)
    for t in np.unique(tenants):
        m = np.where(tenants == t)[0]
        if mode == "row":
            s = rng.integers(0, len(subs_c[t]), len(m))
            base[m] = centres[subs_c[t][s]] + subs_v[t][s]
        else:
            cl = rng.choice(tclusters[t], len(m))
            base[m] = centres[cl] + rng.normal(0, SUB_SPREAD, (len(m), LATENT)).astype(np.float32)
    lat = base + offsets[tenants] + rng.normal(0, ROW_NOISE, (n, LATENT)).astype(np.float32)
    x = lat @ proj.T + rng.normal(0, AMBIENT, (n, DIM)).astype(np.float32)
    x /= np.linalg.norm(x, axis=1, keepdims=True)
    return x.astype(np.float32)

def dsn(port):
    pw = os.environ["PGPASSWORD"]  # never written to the repo
    return f"host=127.0.0.1 port={port} user=postgres dbname=postgres password={pw}"
