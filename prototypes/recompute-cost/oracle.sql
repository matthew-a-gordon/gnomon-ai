-- PROTOTYPE, wipe me. Independent check: as-of read from Versions vs winners computed directly from Contributions.
-- Usage: psql -v nodes_limit=50 -v samples=40 -f oracle.sql
set role gnomon_owner;
with pts as (
  select n.id node_id, n.tenant_id, k.ranked_merge rk,
         timestamptz '2021-01-01' + random() * interval '1826 days' vat,
         now() - random() * interval '720 days' kat
    from (select * from gnomon.node order by random() limit :nodes_limit) n
    join gnomon.kind k on k.tenant_id = n.tenant_id and k.name = n.kind
    cross join generate_series(1, :samples)
), expect as (
  select p.node_id, p.vat, p.kat,
         (select jsonb_object_agg(w.property, w.value) from (
            select distinct on (c.property) c.property, c.value
              from gnomon.contribution c join gnomon.episode e on e.id = c.episode_id
             where c.node_id = p.node_id and c.recorded_at <= p.kat and c.valid_from <= p.vat and c.valid_to > p.vat
             order by c.property, case when p.rk then e.authority else 0 end desc, e.reference_time desc, c.recorded_at desc, c.id desc) w
          where w.value <> 'null'::jsonb) exp
    from pts p
), got as (
  select x.node_id, x.vat, x.kat,
         (select v.props from gnomon.version v where v.node_id = x.node_id and v.valid_from <= x.vat and v.valid_to > x.vat
             and v.recorded_from <= x.kat and v.recorded_to > x.kat) props,
         (select count(*) from gnomon.version v where v.node_id = x.node_id and v.valid_from <= x.vat and v.valid_to > x.vat
             and v.recorded_from <= x.kat and v.recorded_to > x.kat) n
    from expect x
)
select count(*) samples, count(*) filter (where e.exp is not distinct from g.props) agree,
       count(*) filter (where e.exp is distinct from g.props) disagree, count(*) filter (where g.n > 1) multi_version_reads,
       count(*) filter (where g.props is null) empty_reads
  from expect e join got g using (node_id, vat, kat);
