# PROTOTYPE, wipe me. Usage: python tables.py  -> markdown tables from results-*.jsonl
import json, glob, collections
rows = []
for f in sorted(glob.glob("results-*.jsonl")):
    rows += [json.loads(l) for l in open(f)]
rows = [r for r in rows if "recall_mean" in r]
ORDER = ["S100", "S300", "S1000", "M2k", "Mmax", "L200k"]
def key(cls):
    base, mode = cls.split("/")
    base = "Mmax" if base.startswith("Mmax") else base
    return base, mode
def tab(server, table, mode, configs, title):
    sel = [r for r in rows if r["server"] == server and r.get("table", "gnomon.embedding") == table and key(r["cls"])[1] == mode]
    by = collections.defaultdict(dict)
    for r in sel: by[r["config"]][key(r["cls"])[0]] = r
    out = [f"**{title}** ({server}, {table.split('.')[1]}, {mode} queries). Cells: recall@10, p50/p95 ms.", "",
           "| config | " + " | ".join(ORDER) + " |", "|---|" + "---|" * len(ORDER)]
    for c in configs:
        if c not in by: continue
        cells = []
        for k in ORDER:
            r = by[c].get(k)
            cells.append("" if not r else f"{r['recall_mean']:.2f}, {r['p50_ms']:g}/{r['p95_ms']:g}")
        out.append(f"| {c} | " + " | ".join(cells) + " |")
    return "\n".join(out)

if __name__ == "__main__":
    import sys
    cfgs = sorted({r["config"] for r in rows})
    for server in sorted({r["server"] for r in rows}):
        for table in sorted({r.get("table", "gnomon.embedding") for r in rows if r["server"] == server}):
            for mode in ("topical", "near"):
                cs = [c for c in cfgs]
                print(tab(server, table, mode, cs, "All configs"))
                print()
