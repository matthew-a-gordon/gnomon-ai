"""PROTOTYPE, wipe me. Shared synthetic-data definitions for the exact-search prototype."""
import uuid
import numpy as np

DIM = 1536
NTOPIC = 200
SUBS_PER_TOPIC = 10
NSUB = NTOPIC * SUBS_PER_TOPIC
VOCAB = 30000
WORDS_PER_SUB = 60
NOISE = 0.6  # relative noise norm around a subtopic centre


def unit(x):
    return x / np.linalg.norm(x, axis=-1, keepdims=True)


def build_centers():
    """Two-level clusters: 200 topics, each with 10 subtopics. Returns (subs, sub_words)."""
    rng = np.random.default_rng(1234)
    topics = unit(rng.standard_normal((NTOPIC, DIM)).astype(np.float32))
    subs = np.repeat(topics, SUBS_PER_TOPIC, axis=0) + 0.6 * unit(
        rng.standard_normal((NSUB, DIM)).astype(np.float32)
    )
    subs = unit(subs).astype(np.float32)
    words = rng.integers(0, VOCAB, size=(NSUB, WORDS_PER_SUB))
    return subs, words


def zipf_cdf():
    ranks = np.arange(VOCAB) + 10.0
    p = ranks ** -1.05
    p /= p.sum()
    return np.cumsum(p)


VOCAB_STR = np.array([f"w{i:05d}" for i in range(VOCAB)], dtype=object)


def tenant_uuid(i):
    return uuid.UUID(int=(0x5EED << 96) | (i + 1))


def scope_name(j):
    return f"sc{j:06d}"  # always 8 bytes, so binary COPY rows stay fixed width


def make_vecs(rng, subs, sub_idx):
    """Vectors near the given subtopic centres, unit length, float32."""
    noise = rng.standard_normal((len(sub_idx), DIM), dtype=np.float32) * np.float32(NOISE / np.sqrt(DIM))
    x = subs[sub_idx] + noise
    x /= np.linalg.norm(x, axis=1, keepdims=True)
    return x


def plan():
    """Deterministic list of scopes: tenant 0 is the big tenant with four scopes,
    tenant 1 holds the 1M scope, tenants 2.. are small."""
    out = []
    sid = 0

    def add(t, n):
        nonlocal sid
        sid += 1
        nsub = int(np.clip(n // 400, 4, 1000))
        subs = np.random.default_rng(900 + sid).choice(NSUB, nsub, replace=False)
        out.append(dict(id=sid, tenant_idx=t, tenant=str(tenant_uuid(t)), scope=scope_name(sid),
                        n=int(n), subs=[int(s) for s in subs]))

    for n in (1000, 9000, 40000, 150000):
        add(0, n)
    add(1, 1_000_000)
    rng = np.random.default_rng(77)
    for t in range(2, 302):
        for _ in range(int(rng.integers(1, 4))):
            n = int(np.clip(rng.lognormal(np.log(700), 0.9), 100, 4000))
            add(t, n)
    return out
