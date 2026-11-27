-- The performance data set, after prototype 5's generator (gen.py, common.py
-- on `origin/prototype/exact-search`), as set-based SQL. Sections are split
-- on a line of `-- @@`. `{{...}}` names are filled in by perf.rs.
--
-- Texts are unit vectors near the centres of two-level clusters (20 topics
-- of 10 subtopics, subtopics at relative distance 0.6 from their topic, each
-- text at noise norm 0.6 from its centre), made from two pools of noise
-- vectors, so a text is `l2_normalize(centre + a + b)` and no text repeats.
-- Each Node holds a chain of one to three texts over its Recorded time (60%,
-- 30%, 10%), and 30% of texts are held by two Versions in turn, as a
-- Refinement leaves them. Valid time starts unknown for a fifth of the Nodes
-- and ends for 30% of the rest.
set maintenance_work_mem = '512MB';
create schema perf;
create function perf.normal_vec(scale float8, salt int) returns {{ext}}.vector
  language sql volatile as $$
    select array_agg((sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random()) * scale)::real)
             ::real[]::{{ext}}.vector({{dim}})
    from generate_series(1, {{dim}}) d where d + salt * 0 > 0
  $$;
create unlogged table perf.pool (kind text, i int, v {{ext}}.vector({{dim}}),
                                 primary key (kind, i));
insert into perf.pool
  select k, i, perf.normal_vec(0.6 / sqrt(2) / sqrt({{dim}}), i)
  from (values ('a'), ('b')) k(k), generate_series(1, 2048) i;
create table perf.centre (id int primary key, centre {{ext}}.vector({{dim}}));
insert into perf.centre
  select (t - 1) * 10 + s, {{ext}}.l2_normalize(topic.v operator({{ext}}.+) perf.normal_vec(0.6 / sqrt({{dim}}), s))
  from (select t, {{ext}}.l2_normalize(perf.normal_vec(1 / sqrt({{dim}}), t)) v
        from generate_series(1, 20) t) topic,
       generate_series(1, 10) s;
-- @@
create unlogged table perf.tx as
  select id, 1 + floor(random() * 200)::int as c,
         1 + floor(random() * 2048)::int as a, 1 + floor(random() * 2048)::int as b
  from generate_series(1, {{texts}}) id;
create unlogged table perf.node as
  with n as (
    select i, case when r < 0.6 then 1 when r < 0.9 then 2 else 3 end as chain
    from (select i, random() as r from generate_series(1, {{texts}}) i) x),
  c as (select i, chain, sum(chain) over (order by i) as cum from n),
  d as (
    select i, cum - chain as first_text, least(chain, {{texts}} - (cum - chain)) as chain,
           random() as u_unknown, random() as u_from, random() as u_to, random() as u_len,
           random() as u_rec
    from c where cum - chain < {{texts}})
  select i, gen_random_uuid() as uuid, first_text, chain,
         case when u_unknown < 0.2 then null
              else timestamptz '2020-01-01 00:00:00+00' + floor(u_from * 2000) * interval '1 day' end as vf,
         case when u_unknown >= 0.2 and u_to >= 0.7
              then timestamptz '2020-01-01 00:00:00+00' + floor(u_from * 2000) * interval '1 day'
                   + (100 + floor(u_len * 1400)) * interval '1 day' end as vt,
         timestamptz '2024-01-01 00:00:00+00' + floor(u_rec * 600) * interval '1 day' as rstart
  from d;
create unlogged table perf.vers as
  select *, ((row_number() over (order by node_i, seq) - 1) / 500)::int as batch from (
    select f.id, f.node_i, f.text_id, f.seq, f.rf,
           lead(f.rf) over (partition by f.node_i order by f.seq) as rt
    from (
      select e.id, e.node_i, e.text_id, e.seq,
             e.rstart + sum(e.off) over (partition by e.node_i order by e.seq) * interval '1 day' as rf
      from (
        select gen_random_uuid() as id, t.node_i, t.text_id, t.rstart,
               row_number() over (partition by t.node_i order by t.text_id, r.rep) as seq,
               case when row_number() over (partition by t.node_i order by t.text_id, r.rep) = 1
                    then 0 else 1 + floor(random() * 60) end as off
        from (select n.i as node_i, x.id as text_id, n.rstart, 1 + (random() < 0.3)::int as reps
              from perf.node n, generate_series(n.first_text + 1, n.first_text + n.chain) x(id)) t,
             generate_series(1, t.reps) r(rep)) e) f) z;
create index on perf.vers (batch);
-- @@
begin;
set local session_replication_role = replica;
insert into gnomon.node (tenant_id, scope, id, kind)
  select '{{tenant}}', '{{scope}}', uuid, 'company' from perf.node;
insert into gnomon.version (tenant_id, scope, id, node_id, props, valid_from,
                            valid_from_precision, valid_to, valid_to_precision,
                            recorded_from, recorded_to, merge_rule, closed_in)
  select '{{tenant}}', '{{scope}}', v.id, n.uuid,
         jsonb_build_object('name', '{{scope}} text ' || v.text_id),
         coalesce(n.vf, '-infinity'),
         case when n.vf is null then 'unknown' else 'instant' end::gnomon.time_precision,
         coalesce(n.vt, 'infinity'),
         case when n.vt is null then 'unknown' else 'instant' end::gnomon.time_precision,
         v.rf, coalesce(v.rt, 'infinity'), 'recency',
         case when v.rt is null then null else '0'::xid8 end
  from perf.vers v join perf.node n on n.i = v.node_i;
commit;
-- @@
-- @@index-search-text@@
-- @@
-- @@embeddings@@
-- One connection stores the embeddings, in batches of Versions, each as two
-- statements in one transaction: the full vectors, then the quantized rows,
-- whose trigger derives its bits from the full vector stored for the hash.
-- Parallel callers of `gnomon.store_embedding` blocked each other on the
-- shared full-vector rows and on their commits, so the loader does not use
-- them. The triggers and checks of every table still run.
-- per batch
insert into gnomon.embedding (tenant_id, scope, text_hash, embedding)
select distinct on (gv.text_hash) gv.tenant_id, gv.scope, gv.text_hash,
       {{ext}}.l2_normalize(c.centre operator({{ext}}.+) pa.v operator({{ext}}.+) pb.v)
from perf.vers v
  join perf.tx t on t.id = v.text_id
  join perf.centre c on c.id = t.c
  join perf.pool pa on pa.kind = 'a' and pa.i = t.a
  join perf.pool pb on pb.kind = 'b' and pb.i = t.b
  join gnomon.version gv on gv.tenant_id = $1 and gv.id = v.id
where v.batch = $2
order by gv.text_hash
on conflict (tenant_id, scope, text_hash) do nothing
-- then
insert into gnomon.embedding_bit (tenant_id, scope, version_id, text_hash)
select gv.tenant_id, gv.scope, gv.id, gv.text_hash
from perf.vers v
  join gnomon.version gv on gv.tenant_id = $1 and gv.id = v.id
where v.batch = $2
-- @@
-- One Edge from each Node to the next, current and valid always, each cited
-- by the data set's Episode. The tables are analyzed first, so the joins
-- below are planned on the rows just loaded and not on an empty table's
-- defaults.
analyze gnomon.node;
analyze gnomon.version;
analyze gnomon.episode;
begin;
set local session_replication_role = replica;
with nodes as (
  select id, row_number() over (order by id) as i from gnomon.node
  where tenant_id = '{{tenant}}' and scope = '{{scope}}' and kind = 'company'),
pairs as (
  select gnomon.uuid_v7() as edge, a.id as source, b.id as target
  from nodes a join nodes b on b.i = a.i % (select count(*) from nodes) + 1),
edges as (
  insert into gnomon.edge (tenant_id, scope, id, kind, source_id, target_id)
  select '{{tenant}}', '{{scope}}', p.edge, 'edge', p.source, p.target from pairs p returning id),
versions as (
  insert into gnomon.version (tenant_id, scope, id, edge_id, props, valid_from,
                              valid_from_precision, valid_to, valid_to_precision,
                              recorded_from, merge_rule)
  select '{{tenant}}', '{{scope}}', gnomon.uuid_v7(), x.id, '{}', '-infinity', 'unknown',
         'infinity', 'unknown', '2024-01-01', 'recency'
  from edges x returning id)
insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property,
                             recorded_at, episode_reference_time, episode_recorded_at)
select '{{tenant}}', '{{scope}}', v.id, ep.id, 'holds', '2024-01-01',
       ep.reference_time, ep.recorded_at
from versions v cross join gnomon.episode ep
where ep.tenant_id = '{{tenant}}' and ep.scope = '{{scope}}';
commit;
-- @@
drop table perf.vers, perf.tx, perf.node, perf.pool;
drop function perf.normal_vec(float8, int);
-- @@
vacuum (freeze, analyze);
