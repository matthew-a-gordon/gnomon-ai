"""PROTOTYPE, wipe me. Read-path comparison: versions-only copy vs full schema vs after a committed Erasure."""
import bench
base = "gnomon_proto_base"
bench.clone(base, "gnomon_run_full")
bench.clone(base, "gnomon_run_vonly")
c = bench.connect("gnomon_run_vonly").cursor()
c.execute("drop table gnomon.citation, gnomon.contribution, gnomon.episode")
c.execute("vacuum (analyze)")
c.execute("select pg_size_pretty(pg_total_relation_size('gnomon.version')), pg_size_pretty(pg_database_size(current_database()))"); print("vonly sizes", c.fetchone())
for tag, db in (("vonly_a", "gnomon_run_vonly"), ("full_a", "gnomon_run_full"), ("vonly_b", "gnomon_run_vonly"), ("full_b", "gnomon_run_full")):
    r = bench.exp_reads(db, tag); print(tag, {k: (round(v["p50"], 2), round(v["p95"], 2)) for k, v in r.items() if isinstance(v, dict)}, round(r["point_concurrent"]["qps"]))
r = bench.exp_erase(base, "mid", "full", keep=True)   # leaves gnomon_run_mid_full committed
db = "gnomon_run_mid_full"
r = bench.exp_reads(db, "after_erasure_dirty"); print("after_dirty", {k: (round(v["p50"], 2), round(v["p95"], 2)) for k, v in r.items() if isinstance(v, dict)}, round(r["point_concurrent"]["qps"]))
bench.connect(db).execute("vacuum (analyze)")
r = bench.exp_reads(db, "after_erasure_vacuumed"); print("after_vacuumed", {k: (round(v["p50"], 2), round(v["p95"], 2)) for k, v in r.items() if isinstance(v, dict)}, round(r["point_concurrent"]["qps"]))
for d in ("gnomon_run_full", "gnomon_run_vonly", db): bench.drop(d)
