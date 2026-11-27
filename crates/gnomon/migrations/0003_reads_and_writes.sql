-- gnomon 0003: reads and writes. Episodes, Nodes, Edges, Contributions,
-- Versions and Citations, bound statements, Corrections, End of validity and
-- Retraction, Retract Episode, stage records, Summaries and Communities, and
-- the triggers that guard them (ADR-0003, ADR-0007, ADR-0008, ADR-0011,
-- ADR-0014, ADR-0015).
--
-- Episodes. An Episode is an immutable source of its own, not a Node
-- (ADR-0003). Callers may insert and select Episodes and nothing else: no
-- UPDATE and no DELETE. Only Erasure, a definer function in 0005, removes
-- one.
--
-- Nodes, Contributions, Versions and Citations. A write states, for one Node and citing one Episode, a value for each of
-- some properties over one Valid time. Each value is stored as a
-- Contribution, and the Node's Versions are computed from its Contributions
-- (ADR-0007), under the merge rule of its Kind (see 0002).
--
-- Every Version is bi-temporal. Valid time and Recorded time are four scalar
-- timestamptz columns with B-tree indexes (ADR-0004), both half-open,
-- `[from, to)`, with `infinity` for an open bound. Each Valid-time bound
-- carries a precision (ADR-0008). An exclusion constraint keeps a Node's or
-- an Edge's Versions from overlapping in both times at once, with
-- btree_gist's GiST equality on the Tenant and the Node or Edge.
--
-- Callers write these tables as the invoker, under RLS (ADR-0009). They get
-- SELECT and INSERT, with INSERT limited to the columns a caller may choose.
-- On Versions they also get UPDATE of the Recorded-time end column alone, and
-- on Versions and Citations a DELETE that a guard trigger refuses for every
-- row the current transaction did not write. Each Version and Citation
-- records the transaction that wrote it in `written_in`, which an insert
-- default sets and no caller may supply or change. Only Erasure deletes
-- committed history.
--
-- A caller may also insert rows directly, as any SQL client could, so the
-- checks a write depends on run in triggers, not only in the write
-- function: every Contribution value and every Version value is checked
-- against the Node Kind's compiled rules (ADR-0002), every new Version holds
-- the Kind's required properties, and a Citation may be added only to a
-- current Version.
--
-- Bound precision, Refinement and the `uncertain` marker. Each Valid-time
-- bound carries a precision of `unknown`, `year`, `month`,
-- `day` or `instant`, and is stored as the start of that period in UTC
-- (ADR-0008). A write may name any instant inside the period, and the Store
-- truncates it, so the stored bound never depends on the session's time zone.
-- An unknown start is stored as -infinity and reads as open.
--
-- An As-of read marks a result `uncertain` when its valid-at instant falls
-- inside the period of one of the result's imprecise bounds. The stored end
-- of a half-open Valid time is the start of its period, so a result's valid-at
-- instant always lies before its end bound's period, and only the start bound
-- can mark a result.
--
-- A bound statement is an Episode's statement of one Valid-time bound of one
-- Contribution (ADR-0007). It states a bound that was unknown or coarse, or
-- one that contradicts the earlier bound. It moves no row: a Contribution's
-- effective bound is the latest statement made about it, and every
-- recompute reads the effective bounds, so a later write to the Node cannot
-- undo it, and Erasing the statement's Episode brings the earlier bound
-- back. The trigger on bound statements decides, from the earlier bound,
-- whether a statement is a Refinement or a Correction, so no caller can
-- label one as the other:
--
--   Refinement  the earlier bound was unknown, or the statement narrows its
--               precision inside the same period. The earlier Version was
--               incomplete, not wrong.
--   Correction  any other change. A bound outside the earlier period, or a
--               coarser one, contradicts the earlier Version.
--
-- Either way the earlier Version is closed in Recorded time and stays
-- readable As-of before the statement. Callers get no new UPDATE or DELETE:
-- bound statements are select and insert only, like Contributions.
--
-- Every Version also carries the hash of its text, the property values that
-- Search and embeddings are derived from. Versions with equal text share the
-- hash, so a Refinement, which states no new text, shares the earlier
-- Version's and needs no new embedding. The column is generated, so no
-- caller can set it.
--
-- Edges, their Sentences, traversal and the batch As-of read. An Edge is a stable identity for a directed relationship between two
-- Nodes, of an Edge Kind, in one Scope. It joins the Nodes themselves, not
-- their Versions. What is known about it is held in its Versions, which are
-- computed from its Contributions exactly as a Node's are (ADR-0007): the
-- Contribution, Version, Citation and bound statement tables hold the rows
-- of Edges beside those of Nodes. Each such row names its Node in
-- `node_id` or its Edge in `edge_id`, never both, and `versioned_id` is
-- whichever it names. The merge, the guard triggers, bound precision and
-- ranked merge read `versioned_id`, so they cover Edge Versions with no
-- second copy of their rules. Callers may insert `edge_id` beside `node_id`.
--
-- An Edge holds over a Valid time. Every write to an Edge records, besides
-- the property values it states, a Contribution of the built-in property
-- `holds` with the value true over the write's Valid time. An Edge Version
-- exists only where `holds` wins, and its values are the Edge Kind's
-- properties there: `holds` never appears among them, but the Citations name
-- it, so a write that states no property value still cites its Episode. A
-- write to an Edge may state no property value at all. An End of validity of
-- an Edge is a Contribution of `holds` with a JSON null.
--
-- The Store checks only that both endpoints exist in the Tenant and that the
-- Edge Kind allows their Kinds (ADR-0008). Temporal referential integrity is
-- not enforced: a Node's Valid time belongs to each Version, so there is no
-- lifespan to check an Edge against. An Edge lives in one Scope, and each
-- endpoint may be in any Scope of the Tenant that the caller can see. A
-- foreign key on the Tenant and the Node refuses an endpoint in another
-- Tenant whatever RLS allows, and the insert trigger, which runs as the
-- caller under RLS, refuses a Node the caller cannot see. A Node with no
-- Versions stays as a bare identity while any Edge joins it: the foreign keys
-- keep it.
--
-- A Sentence states in plain language what one Edge Version says. The
-- Pipeline writes it after the Version commits, from the Version as
-- committed (ADR-0013), so it is held outside the Version's values, in a
-- table keyed by the Edge Version, and a write to an Edge takes none. Reads
-- and traversal return each Edge Version's Sentence, or none until one is
-- stored with `gnomon.store_sentence` (see 0004).
--
-- Correction, End of validity and Retraction. A Correction, an End of validity and a Retraction are statements of the
-- Episode that makes them, held like Contributions (ADR-0007). Each has a row
-- in `gnomon.statement` that records its kind, its Episode, its Valid time
-- and, for an End of validity or a Retraction, the Version it ended or
-- retracted. The merge reads no statement row: a statement takes effect
-- through Contributions that name it in `contribution.statement_id`, so
-- `gnomon.recompute_versions`, ranked merge, Citations and the guard triggers
-- cover statements with no second copy of their rules, and Erasure
-- recomputes through the same merge.
--
--   Correction        Contributions of the values it states, over the Valid
--                     time it states them for. The values replace the earlier
--                     Version's where the merge lets them win: the earlier
--                     Version is closed in Recorded time and a new one opens,
--                     so the earlier Version stays readable As-of before the
--                     Correction.
--   End of validity   Contributions of JSON null, from the stated instant to
--                     the end of the Version that held across it, for each
--                     property of that Version. For an Edge it is one
--                     Contribution of the built-in `holds`, so the Edge holds
--                     no Version from that instant whatever property values
--                     remain.
--   Retraction        Contributions of JSON null over the whole Valid time of
--                     a current Version, for each property of that Version
--                     and, for an Edge, `holds`. The Version is closed in
--                     Recorded time and stays readable As-of before the
--                     Retraction.
--
-- Because a statement is a set of Contributions, ranked merge applies to it
-- as it does to values: a statement from a lower-Authority Episode loses to
-- the values a higher-Authority Episode supplied where their Valid times
-- overlap, stays held, and takes effect when that Episode is erased
-- (ADR-0007). Without ranked merge a statement wins only when its Episode's
-- Reference time is not before the values it ends, as any Contribution does.
--
-- `bound_statement` is the other statement table. It names one Contribution
-- and one bound, and the merge reads it. `statement` names a Node or an
-- Edge, and the merge reads the Contributions that carry its
-- `statement_id`. Erasure deletes, for an Episode: its `contribution` rows,
-- which include those of its statements, then its `statement` rows, then
-- its `bound_statement` rows. A statement's Contributions are deleted with
-- it, by the foreign key. Callers get no UPDATE or DELETE right on either
-- table, and the statement functions run as the invoker (ADR-0009).
--
-- Retract Episode. It records a Retraction for every current Version cited only
-- by the Episode, and keeps history readable (ADR-0003). It is not Erasure:
-- it deletes no Contribution, no Citation, no statement and no Episode, so an
-- As-of read at a known-at instant before it returns what it returned then,
-- and Erasure is still the only route that deletes.
--
-- A Retraction is a statement (`gnomon.statement`, kind 'retraction'), held
-- like a Contribution, so Retract Episode adds no way to end a Version. It is written by the retracted Episode, at its Authority and
-- Reference time, and for an Edge it is a null for the built-in `holds`. A
-- Version that another Episode also cites is not touched, and ranked merge
-- applies to the Retractions as it does to any statement: a Retraction from
-- a lower-Authority Episode does not end values a higher-Authority Episode
-- supplied where their Valid times overlap (ADR-0007).
--
-- The Episode's own statements are held too (a Correction, an End of
-- validity or a Retraction of the Episode that lost, or that took effect).
-- Retract Episode retracts the Episode, so none of them counts any more, and
-- none takes effect if a higher-Authority Episode is erased later.
-- `gnomon.episode_retraction` records that the Episode is retracted, and
-- the merge leaves out the
-- Contributions of an Episode's statements and its bound statements once the
-- row exists, apart from the Retractions Retract Episode itself made
-- (`statement.retracts_episode`). The statements stay in their tables, so
-- history stays readable; only the merge stops reading them. Contributions of
-- values, not of statements, still count: they are what the Versions cite, and
-- the Retractions above are what ends the Versions that only the Episode
-- cited.
--
-- Erasure of a retracted Episode deletes its `episode_retraction` row with
-- the rest: the row cascades from the Episode. `retract_episode` runs as the
-- invoker (ADR-0009), and callers get no UPDATE or DELETE right.
--
-- Stage records. The Pipeline's `add_episode` is one resumable call (ADR-0011). Each stage
-- commits its output to the Store together with its own stage record, so a
-- retry resumes at the first unfinished stage. The optional server queue is
-- the same table: a worker claims a pending or retry-due record with a lease,
-- runs the stage, and finishes the record in the stage's own transaction.
--
-- A stage record holds the Episode, the stage name, a status, an attempt
-- count, the last error, a retry time and a lease time. Its usage counts,
-- one row per model, hold the model name, the call count, the input and
-- output tokens and the provider's reported cost when given. Neither table
-- has a column for model output or any other text (ADR-0011): anything
-- derived from an Episode must be reachable by Erasure (ADR-0003), and model
-- output would be one more copy of the Episode to delete. So every text
-- column but the Scope is an identifier, refused by a check constraint unless
-- it matches a fixed pattern with no spaces and a fixed maximum length. The
-- Scope is the Episode's own, which the foreign key to the Episode checks.
-- The last error is a code, such as `rate_limited` or `output.invalid_json`,
-- and an optional number, such as the provider's HTTP status. It has no
-- message, because a provider's message, or a parser's, often quotes its
-- input or the model's output.
--
-- Status. A record is
--
--   pending    written, not yet claimed;
--   running    claimed, with a lease until `lease_until`;
--   retry      failed, and due again at `retry_at`;
--   succeeded  finished, in the transaction that committed the stage's output;
--   failed     failed for the last time. It stays visible as failed.
--
-- A claim (`gnomon.claim_stage`) takes one record that is pending, retry-due
-- or running with an expired lease, sets it running with a lease, counts the
-- attempt and returns it. The Store commits the claim as its own call. The
-- claim locks the record with FOR UPDATE SKIP LOCKED, so two concurrent
-- claimers never get the same record: the second skips the record the first
-- holds and takes another, or none. Every time is the database's clock,
-- `now()`, so workers on hosts whose clocks differ agree on when a lease
-- expires and when a retry is due. A lease of zero expires at once.
--
-- A finish (`gnomon.finish_stage`) names the attempt its claim returned. It
-- finishes the record only while the record is still running at that
-- attempt, and otherwise fails with SQLSTATE GNS01. A worker whose lease
-- expired and whose record another worker claimed since cannot finish it, and
-- the stage output in the same transaction rolls back with it. A worker whose
-- lease expired but whom no one replaced still finishes.
--
-- Isolation. Both tables carry the Tenant and the Episode's Scope, and RLS
-- checks both for every caller (ADR-0005). The functions are invokers under
-- RLS (ADR-0009). Callers
-- may insert and select, and update only the columns a claim or a finish
-- changes. They may not delete. Only Erasure deletes a stage record.
--
-- Erasure. `gnomon.erase_episode_rows` deletes the Episode's usage counts
-- and stage records. Erase Scope calls it for each of its Episodes, so a
-- Scope's Erasure removes them too. Erase
-- Node and Erase Edge leave Episodes, and their stage records, alone. The
-- definer role gets the Tenant and Scope policies and SELECT and DELETE on
-- both tables. Retract Episode changes neither table.
--
-- Summaries and Communities. The Pipeline writes a Summary of a Node, generated prose describing it,
-- and a Community of Nodes, a group of closely connected Nodes with a
-- generated Summary of the group (GLOSSARY.md). The Store writes and reads
-- them with their Citations under the same Tenant and Scope rules as every
-- other row. It makes no model calls: the text is the Pipeline's.
--
-- One Scope (ADR-0014). A Summary or Community belongs to one Scope and
-- cites only Episodes of that Scope. The foreign key from a Citation to its
-- Episode carries the Scope of the Summary or Community, so a Citation of an
-- Episode in another Scope is refused whoever writes it. A Summary describes
-- one Node the writer can see, in any of the writer's Scopes: a Node in Scope
-- A that Edges in Scope B join may have a Summary in Scope B too, built from
-- Scope B's Episodes, which is how a Node cited from two Scopes has one
-- Summary per Scope. A Community is detected over the Nodes and Edges of one
-- Scope, so every Node it holds is in its own Scope. The foreign key from a
-- member to its Node carries the Community's Scope, so a Node of another
-- Scope is refused, even one the writer can see.
--
-- Versioned by Recorded time (ADR-0015). Each row carries a Recorded time,
-- which only gnomon assigns, and `as_of_valid`, the valid-at instant the
-- Pipeline wrote it for. A read at `(v, k)` finds a row only when its
-- Recorded time contains `k` and `v >= as_of_valid`. Regenerating one closes
-- the old row at the start of the current transaction and adds a new one, so
-- nothing is deleted and a read at an earlier known-at instant still finds
-- the old row. A Node has at most one current Summary in each Scope. A new
-- Community names the current Communities it replaces, which it closes.
-- Replacing a row the current transaction wrote deletes it instead, as for
-- Versions (ADR-0015): closing a row at the instant it opened would leave it
-- an empty Recorded time, so a second write in one transaction replaces the
-- first (ADR-0008, ADR-0009). No committed row is deleted.
--
-- Who writes (ADR-0009). Callers write through `gnomon.write_summary` and
-- `gnomon.write_community`, invokers under RLS. A caller may also insert rows directly, as any SQL client
-- could, so the checks a write depends on are the tables' own: RLS, foreign
-- keys that carry the Scope, insert grants on the columns a caller may
-- choose, so that the ID, the Recorded time and the transaction mark always
-- take their defaults, and triggers that
--
--   * refuse a Summary of a Node the caller cannot see, and a Summary whose
--     Recorded time would overlap another of the same Node and Scope;
--   * refuse a Citation or a member added to a row an earlier transaction
--     wrote, so a row's Citations and members are fixed when it commits;
--   * refuse, at commit, a Summary with no Citation and a Community with no
--     Citation or no member;
--   * let a row's Recorded-time end change only by closing an open row at
--     the start of the current transaction;
--   * let a row be deleted only by the transaction that wrote it.
--
-- Both write functions take the advisory lock of every Node they describe or
-- hold (`gnomon.lock_for_write`), in order of ID, as an Erasure does, so a
-- write and an Erasure of the same Node serialise. A Citation's foreign key
-- takes a share lock on its Episode's row, which Erase Episode locks FOR
-- UPDATE, so a write citing an Episode and its Erasure serialise too.
--
-- Erasure (ADR-0003). Every Summary and Community that cites an erased
-- Episode goes, closed or open, with its Citations and members:
-- `gnomon.erase_episode_rows` deletes them before the Episode. Erasing a
-- Node removes every Summary of the Node, whatever its Scope, and every
-- Community that holds it: `gnomon.erase_versioned_rows` deletes them.
-- Erasing a Scope removes every Summary and Community in it, because each
-- cites at least one of the Scope's Episodes, and every Summary of one of its
-- Nodes in another Scope. Both go through
-- `gnomon.erase_summaries_and_communities`, which also deletes the rows
-- Search derives from a Summary's or Community's text. A Retraction changes
-- neither table: a Citation of a retracted Episode still counts.
--
-- Reaching Summaries the caller cannot see. A Summary of a Node in Scope B
-- may describe a Node in Scope A. Erasing the Node, or Scope A, must remove
-- it, even when the caller cannot see Scope B, because the foreign key on the
-- Node would refuse the Node's delete while it stood. So the definer gets the
-- reach it has over Edges (ADR-0009, see 0005): it may read and delete
-- a Summary, in any Scope of the Tenant, of a Node its own Erasure listed in
-- `gnomon.erasure_reach`, and that Summary's Citations. No caller role holds
-- these policies, and a listed Node is visible only to the transaction that
-- listed it. Communities need no reach, because their Nodes are in their own
-- Scope, which the caller of the Erasure sees.
--
-- A Node that an Erasure would leave bare and delete stays as a bare identity
-- while a Summary of it, or a Community holding it, still names it, as it
-- does while an Edge still joins it. Only a Summary that cites no erased
-- Episode can name it then.

-- Whether the current user is the NOLOGIN definer role, which is true only
-- inside a function that role owns. Erasure is the one such function that
-- writes the tables whose guards this lets past. Kind registration and the
-- merge-rule setter run as the role too, and write only the Ontology.
create function gnomon.running_as_definer() returns boolean
  language sql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$ select current_user = 'gnomon_definer'::name $$;

-- Store functions run as the invoker with an empty search path, so RLS
-- applies to every call and nothing can be resolved through a caller's
-- search path (ADR-0009).
create function gnomon.record_episode(
  p_tenant_id uuid,
  p_scope text,
  p_content text,
  p_reference_time timestamptz,
  p_authority integer
) returns uuid
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    insert into gnomon.episode (tenant_id, scope, id, content, reference_time, authority)
    values (p_tenant_id, p_scope, gnomon.uuid_v7(), p_content, p_reference_time, p_authority)
    returning id
  $$;

create function gnomon.read_episode(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    tenant_id uuid,
    scope text,
    id uuid,
    content text,
    reference_time timestamptz,
    authority integer,
    recorded_at timestamptz
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select e.tenant_id, e.scope, e.id, e.content, e.reference_time, e.authority, e.recorded_at
    from gnomon.episode e
    where e.tenant_id = p_tenant_id and e.id = p_episode_id
  $$;

-- Checks one value of a property of a Node of `p_kind` against the Kind's
-- compiled rule (ADR-0002). A null ends the property and is not checked, but
-- the Kind must still have the property.
create function gnomon.check_node_value(p_tenant_id uuid, p_kind text, p_property text, p_value jsonb)
  returns void
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    rule jsonpath := coalesce(
      (select x.rule from gnomon.kind_rule x
       where x.tenant_id = p_tenant_id and x.kind = p_kind and x.property = p_property),
      (select b.rule from gnomon.node_builtin_rules() b where b.property = p_property)
    );
  begin
    if rule is null then
      raise exception using errcode = 'GNW01',
        message = format('Kind "%s" has no property "%s"', p_kind, p_property);
    end if;
    if p_value <> 'null'::jsonb and not jsonb_path_match(p_value, rule) then
      raise exception using errcode = 'GNW01',
        message = format(
          'the value %s of property "%s" does not satisfy Kind "%s"', p_value, p_property, p_kind);
    end if;
  end
  $$;

-- The precision of a bound a write names without one: an instant, or
-- unknown for an open bound.
create function gnomon.instant_or_unknown(p_bound timestamptz)
  returns gnomon.time_precision
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case when p_bound in ('-infinity', 'infinity') then 'unknown' else 'instant' end
      ::gnomon.time_precision
  $$;

-- Checks a new Node's Kind: the root Kind `node`, or a Node Kind the Tenant
-- registered.
create function gnomon.node_check_kind() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if new.kind = 'node' then
      return new;
    end if;
    if not exists (
      select from gnomon.kind k
      where k.tenant_id = new.tenant_id and k.name = new.kind and k.kind_of = 'node'
    ) then
      raise exception using errcode = 'GNW01',
        message = format('"%s" is not a Node Kind of the Tenant', new.kind);
    end if;
    return new;
  end
  $$;

create trigger node_check_kind before insert on gnomon.node
  for each row execute function gnomon.node_check_kind();

-- A Contribution's value is checked against its Kind's rules. One that names a statement
-- must be the statement's own: the same Node or Edge, Episode, Valid time and
-- Recorded time, and a JSON null unless the statement is a Correction.
create function gnomon.contribution_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_kind text;
    s gnomon.statement;
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Contribution''s Recorded time is the start of its write''s transaction';
    end if;
    if new.node_id is not null then
      select n.kind into v_kind
        from gnomon.node n
        where n.tenant_id = new.tenant_id and n.scope = new.scope and n.id = new.node_id;
      if found then
        perform gnomon.check_node_value(new.tenant_id, v_kind, new.property, new.value);
      end if;
    elsif new.edge_id is not null then
      select e.kind into v_kind
        from gnomon.edge e
        where e.tenant_id = new.tenant_id and e.scope = new.scope and e.id = new.edge_id;
      if found then
        perform gnomon.check_edge_value(new.tenant_id, v_kind, new.property, new.value);
      end if;
    end if;
    if new.statement_id is not null then
      select * into s from gnomon.statement x
        where x.tenant_id = new.tenant_id and x.scope = new.scope and x.id = new.statement_id;
      if found and (
        (s.node_id, s.edge_id, s.episode_id) is distinct from
          (new.node_id, new.edge_id, new.episode_id)
        or (s.valid_from, s.valid_from_precision, s.valid_to, s.valid_to_precision)
          is distinct from
          (new.valid_from, new.valid_from_precision, new.valid_to, new.valid_to_precision)
        or s.recorded_at <> new.recorded_at
        or (s.kind <> 'correction' and new.value <> 'null'::jsonb)
      ) then
        raise exception using errcode = 'GNW01',
          message = 'a Contribution of a statement is of the statement''s Node or Edge, '
            'Episode and Valid time, and an End of validity or a Retraction states only null';
      end if;
    end if;
    -- A Node, Edge, statement or Episode the caller cannot see fails a
    -- foreign key or the policy, and a row naming both or neither Node and
    -- Edge fails the check constraint.
    return new;
  end
  $$;

create trigger contribution_check_insert before insert on gnomon.contribution
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.contribution_check_insert();

-- A new Version opens when its write's transaction started, is marked as
-- written by that transaction, records the rule of its Node's or Edge's
-- Kind, holds only values that Kind's rules accept and holds every property
-- the Kind requires. An Edge Version's values never hold the built-in
-- `holds`.
create function gnomon.version_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_kind text;
    v_of text;
    v_rule text;
    v_required jsonb;
    missing text;
    k text;
    v jsonb;
  begin
    if new.recorded_from is distinct from transaction_timestamp()
      or new.recorded_to is distinct from 'infinity'
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version''s Recorded time starts when its transaction started, and is open';
    end if;
    if new.written_in is distinct from pg_current_xact_id() or new.closed_in is not null then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version is marked as written by the current transaction';
    end if;
    if new.node_id is not null then
      v_of := 'Node';
      select n.kind into v_kind
        from gnomon.node n
        where n.tenant_id = new.tenant_id and n.scope = new.scope and n.id = new.node_id;
    elsif new.edge_id is not null then
      v_of := 'Edge';
      select e.kind into v_kind
        from gnomon.edge e
        where e.tenant_id = new.tenant_id and e.scope = new.scope and e.id = new.edge_id;
    end if;
    if v_kind is null then
      return new; -- The foreign keys, the check or the policy refuse it.
    end if;
    v_rule := gnomon.kind_merge_rule(new.tenant_id, v_kind);
    if new.merge_rule::text is distinct from v_rule then
      raise exception using errcode = 'GNW01',
        message = format('a new Version records the merge rule of its %s''s Kind, %s', v_of, v_rule);
    end if;
    for k, v in select e.key, e.value from jsonb_each(new.props) e order by e.key loop
      if v = 'null'::jsonb then
        raise exception using errcode = 'GNW01',
          message = format('a Version holds no null; property "%s" is null', k);
      end if;
      if v_of = 'Node' then
        perform gnomon.check_node_value(new.tenant_id, v_kind, k, v);
      elsif exists (select from gnomon.edge_builtin_rules() b where b.property = k) then
        raise exception using errcode = 'GNW01',
          message = format('an Edge Version''s values never hold the built-in property "%s"', k);
      else
        perform gnomon.check_edge_value(new.tenant_id, v_kind, k, v);
      end if;
    end loop;
    select x.properties -> 'required' into v_required
      from gnomon.kind x where x.tenant_id = new.tenant_id and x.name = v_kind;
    select r into missing
      from jsonb_array_elements_text(coalesce(v_required, '[]')) r
      where not new.props ? r
      limit 1;
    if missing is not null then
      raise exception using errcode = 'GNW01',
        message = format(
          'the write leaves required property "%s" of Kind "%s" without a value over [%s, %s)',
          missing, v_kind, new.valid_from, new.valid_to);
    end if;
    return new;
  end
  $$;

create trigger version_check_insert before insert on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_check_insert();

-- The guard on Versions (ADR-0009). The Node and Edge columns are among
-- those that never change, and Edge Versions get the same guard.
create function gnomon.version_guard_update() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if (new.tenant_id, new.scope, new.id, new.node_id, new.edge_id, new.props, new.valid_from,
        new.valid_from_precision, new.valid_to, new.valid_to_precision, new.recorded_from,
        new.merge_rule, new.written_in, new.closed_in)
      is distinct from
       (old.tenant_id, old.scope, old.id, old.node_id, old.edge_id, old.props, old.valid_from,
        old.valid_from_precision, old.valid_to, old.valid_to_precision, old.recorded_from,
        old.merge_rule, old.written_in, old.closed_in)
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Version is immutable apart from the end of its Recorded time';
    end if;
    if old.recorded_to = 'infinity'
      and new.recorded_to = transaction_timestamp()
      and old.recorded_from < new.recorded_to
    then
      new.closed_in := pg_current_xact_id();
      return new;
    end if;
    if old.closed_in = pg_current_xact_id() and new.recorded_to = 'infinity' then
      new.closed_in := null;
      return new;
    end if;
    raise exception using errcode = 'insufficient_privilege',
      message = 'a Version''s Recorded time may only be closed at the start of the current '
        'transaction, or reopened by the transaction that closed it';
  end
  $$;

create trigger version_guard_update before update on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_guard_update();

-- Versions and Citations are deleted only by the transaction that wrote
-- them, which is how a second write to a Node in one transaction replaces
-- the first in place (ADR-0008). Committed history is deleted only by
-- Erasure.
create function gnomon.guard_delete() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if old.written_in is distinct from pg_current_xact_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = format(
          'a %s may be deleted only by the transaction that wrote it', tg_argv[0]);
    end if;
    return old;
  end
  $$;

create trigger version_guard_delete before delete on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Version');

create trigger citation_guard_delete before delete on gnomon.citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');

-- A Citation is added only to a current Version, so closed history keeps
-- the Citations it had, and it is recorded when its write's transaction
-- started.
create function gnomon.citation_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp()
      or new.written_in is distinct from pg_current_xact_id()
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Citation is recorded when its write''s transaction started';
    end if;
    if not exists (
      select from gnomon.version v
      where v.tenant_id = new.tenant_id and v.scope = new.scope and v.id = new.version_id
        and v.recorded_to = 'infinity'
    ) then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Citation may be added only to a current Version';
    end if;
    return new;
  end
  $$;

create trigger citation_check_insert before insert on gnomon.citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.citation_check_insert();

-- The key of a Node's advisory lock. Writes to a Node, and Erasure, take it
-- for the rest of the transaction, so they serialise on each Node (ADR-0007).
create function gnomon.node_lock_key(p_tenant_id uuid, p_node_id uuid)
  returns bigint
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$ select pg_catalog.hashtextextended(p_tenant_id::text || '/' || p_node_id::text, 0) $$;

-- Creates a Node of a Kind in a Scope and returns its ID, a UUID v7. The Node
-- holds no Version until a write states a value for it.
create function gnomon.create_node(p_tenant_id uuid, p_scope text, p_kind text)
  returns uuid
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    insert into gnomon.node (tenant_id, scope, kind) values (p_tenant_id, p_scope, p_kind)
    returning id
  $$;

-- Lists the Citations of a Version: each Episode it was derived from, with
-- the property and the place.
create function gnomon.list_citations(p_tenant_id uuid, p_version_id uuid)
  returns table (
    episode_id uuid,
    property text,
    place text,
    recorded_at timestamptz,
    episode_recorded_at timestamptz
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select c.episode_id, c.property, c.place, c.recorded_at, c.episode_recorded_at
    from gnomon.citation c
    where c.tenant_id = p_tenant_id and c.version_id = p_version_id
    order by c.property, c.episode_recorded_at, c.episode_id, c.place
  $$;

-- The start of the period of `p_at` at a precision, in UTC.
create function gnomon.period_start(p_at timestamptz, p_precision gnomon.time_precision)
  returns timestamptz
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case p_precision
      when 'year' then date_trunc('year', p_at at time zone 'UTC') at time zone 'UTC'
      when 'month' then date_trunc('month', p_at at time zone 'UTC') at time zone 'UTC'
      when 'day' then date_trunc('day', p_at at time zone 'UTC') at time zone 'UTC'
      else p_at
    end
  $$;

-- The end of the period that starts at `p_start`: the start of the next
-- period. An instant has no extent, so its period ends where it starts.
create function gnomon.period_end(p_start timestamptz, p_precision gnomon.time_precision)
  returns timestamptz
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case p_precision
      when 'year' then (p_start at time zone 'UTC' + interval '1 year') at time zone 'UTC'
      when 'month' then (p_start at time zone 'UTC' + interval '1 month') at time zone 'UTC'
      when 'day' then (p_start at time zone 'UTC' + interval '1 day') at time zone 'UTC'
      else p_start
    end
  $$;

create function gnomon.read_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    node_id uuid,
    scope text,
    kind text,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select v.id, v.node_id, v.scope, n.kind, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule,
           v.valid_from_precision in ('year', 'month', 'day')
             and p_valid_at < gnomon.period_end(v.valid_from, v.valid_from_precision),
           v.text_hash
    from gnomon.version v
    join gnomon.node n on n.tenant_id = v.tenant_id and n.id = v.node_id
    where v.tenant_id = p_tenant_id and v.node_id = p_node_id
      and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
  $$;

-- Whether stating a bound is a Refinement, a Correction, or no change.
create function gnomon.classify_bound(
  p_old_at timestamptz,
  p_old_precision gnomon.time_precision,
  p_new_at timestamptz,
  p_new_precision gnomon.time_precision
) returns text
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case
      when p_old_at = p_new_at and p_old_precision = p_new_precision then 'unchanged'
      when p_old_precision = 'unknown' then 'refinement'
      when p_new_precision > p_old_precision
        and p_new_at >= p_old_at and p_new_at < gnomon.period_end(p_old_at, p_old_precision)
        then 'refinement'
      else 'correction'
    end
  $$;

-- A bound statement's insert check, for a Contribution of a Node or of an
-- Edge. The statement names the Node or Edge of its
-- Contribution, and a direct insert takes that one's lock.
create function gnomon.bound_statement_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    earlier record;
    other record;
    empty boolean;
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a bound statement''s Recorded time is the start of its write''s transaction';
    end if;
    if new.bound_at in ('-infinity', 'infinity') or new.bound_precision = 'unknown' then
      return new; -- The check constraints refuse it.
    end if;
    if num_nonnulls(new.node_id, new.edge_id) <> 1 then
      return new; -- The check constraint refuses it.
    end if;
    -- A direct insert serialises with writes and Erasure (ADR-0007). The
    -- generated `versioned_id` is not yet set in a before trigger.
    perform pg_catalog.pg_advisory_xact_lock(
      gnomon.node_lock_key(new.tenant_id, coalesce(new.node_id, new.edge_id)));
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = new.tenant_id and c.id = new.contribution_id
        and (c.node_id, c.edge_id) is distinct from (new.node_id, new.edge_id)
    ) then
      raise exception using errcode = 'GNW01',
        message = format('a statement names the %s of the Contribution it is about',
                         case when new.edge_id is null then 'Node' else 'Edge' end);
    end if;
    select * into earlier from gnomon.contribution_bound(
      new.tenant_id, new.contribution_id, new.side);
    if earlier.bound_at is null then
      return new; -- A Contribution the caller cannot see fails the foreign key.
    end if;
    new.bound_at := gnomon.period_start(new.bound_at, new.bound_precision);
    new.kind := gnomon.classify_bound(
      earlier.bound_at, earlier.bound_precision, new.bound_at, new.bound_precision);
    if new.kind = 'unchanged' then
      raise exception using errcode = 'GNW01',
        message = 'the statement gives the bound the Contribution already has';
    end if;
    select * into other from gnomon.contribution_bound(
      new.tenant_id, new.contribution_id, case new.side when 'from' then 'to' else 'from' end);
    empty := case new.side
      when 'from' then new.bound_at >= other.bound_at
      else other.bound_at >= new.bound_at
    end;
    if empty then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty',
          case new.side when 'from' then new.bound_at else other.bound_at end,
          case new.side when 'from' then other.bound_at else new.bound_at end);
    end if;
    return new;
  end
  $$;

create trigger bound_statement_check_insert before insert on gnomon.bound_statement
  for each row execute function gnomon.bound_statement_check_insert();

-- Checks one value of a property of an Edge of `p_kind`, as
-- `gnomon.check_node_value` does for a Node. The built-in comes first.
create function gnomon.check_edge_value(p_tenant_id uuid, p_kind text, p_property text, p_value jsonb)
  returns void
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    rule jsonpath := coalesce(
      (select b.rule from gnomon.edge_builtin_rules() b where b.property = p_property),
      (select x.rule from gnomon.kind_rule x
       where x.tenant_id = p_tenant_id and x.kind = p_kind and x.property = p_property)
    );
  begin
    if rule is null then
      raise exception using errcode = 'GNW01',
        message = format('Kind "%s" has no property "%s"', p_kind, p_property);
    end if;
    if p_value <> 'null'::jsonb and not jsonb_path_match(p_value, rule) then
      raise exception using errcode = 'GNW01',
        message = format(
          'the value %s of property "%s" does not satisfy Kind "%s"', p_value, p_property, p_kind);
    end if;
  end
  $$;

-- The insert check on Edges, with the reserved Kinds among the Edge Kinds
-- of every Tenant. An Edge of a symmetric Kind joins two
-- different Nodes, and once both are checked its endpoints are stored in
-- the canonical order, the lower ID as the source, whatever order the caller
-- gave, so the unique index below sees one Edge per pair.
create function gnomon.edge_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    allowed record;
    v_end record;
    v_kind text;
  begin
    select r.source_kinds, r.target_kinds into allowed
      from gnomon.root_kinds() r where r.name = new.kind and r.kind_of = 'edge';
    if not found then
      select r.source_kinds, r.target_kinds into allowed
        from gnomon.reserved_kinds() r where r.name = new.kind;
    end if;
    if not found then
      select k.source_kinds, k.target_kinds into allowed
        from gnomon.kind k
        where k.tenant_id = new.tenant_id and k.name = new.kind and k.kind_of = 'edge';
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('"%s" is not an Edge Kind of the Tenant', new.kind);
      end if;
    end if;
    if gnomon.is_symmetric_kind(new.kind) and new.source_id = new.target_id then
      raise exception using errcode = 'GNW01',
        message = format('an Edge of the symmetric Kind "%s" joins two different Nodes', new.kind);
    end if;
    for v_end in
      select * from (values
        ('source', new.source_id, allowed.source_kinds, 'start at'),
        ('target', new.target_id, allowed.target_kinds, 'end at')
      ) x(side, node_id, kinds, verb)
    loop
      select n.kind into v_kind
        from gnomon.node n where n.tenant_id = new.tenant_id and n.id = v_end.node_id;
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('the %s Node %s is not visible to the caller', v_end.side, v_end.node_id);
      end if;
      if not gnomon.kind_lineage(new.tenant_id, v_kind) && v_end.kinds then
        raise exception using errcode = 'GNW01',
          message = format(
            'Edge Kind "%s" may not %s a Node of Kind "%s"; it allows %s',
            new.kind, v_end.verb, v_kind, array_to_string(v_end.kinds, ', '));
      end if;
    end loop;
    if gnomon.is_symmetric_kind(new.kind) then
      select least(new.source_id, new.target_id), greatest(new.source_id, new.target_id)
        into new.source_id, new.target_id;
    end if;
    return new;
  end
  $$;

create trigger edge_check_insert before insert on gnomon.edge
  for each row execute function gnomon.edge_check_insert();

-- A Sentence belongs to an Edge Version.
create function gnomon.sentence_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if not exists (
      select from gnomon.version v
      where v.tenant_id = new.tenant_id and v.scope = new.scope and v.id = new.version_id
        and v.edge_id is not null
    ) then
      raise exception using errcode = 'GNW01',
        message = format('a Sentence states what an Edge Version says; %s is not one', new.version_id);
    end if;
    return new;
  end
  $$;

create trigger sentence_check_insert before insert on gnomon.sentence
  for each row execute function gnomon.sentence_check_insert();

-- Opens a write to a Node or an Edge: checks the Tenant against the
-- caller's, takes the lock of the Node or Edge, and returns its Scope and
-- Kind. An Edge's lock has the key `gnomon.node_lock_key` gives its ID, so
-- writes to one Edge serialise as writes to one Node do (ADR-0007).
create function gnomon.lock_for_write(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  out scope text,
  out kind text
)
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a write may write only in the caller''s own Tenant';
    end if;
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_id));
    if p_is_edge then
      select e.scope, e.kind into scope, kind
        from gnomon.edge e where e.tenant_id = p_tenant_id and e.id = p_id;
    else
      select n.scope, n.kind into scope, kind
        from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_id;
    end if;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('%s %s is not visible to the caller',
                         case when p_is_edge then 'Edge' else 'Node' end, p_id);
    end if;
  end
  $$;

-- The Kind of the Node or Edge a Contribution is of.
create function gnomon.contribution_kind(p_tenant_id uuid, p_node_id uuid, p_edge_id uuid)
  returns text
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select coalesce(
      (select n.kind from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_node_id),
      (select e.kind from gnomon.edge e where e.tenant_id = p_tenant_id and e.id = p_edge_id)
    )
  $$;

-- The bound a Contribution has now, for the Contribution of a Node or of
-- an Edge.
create function gnomon.contribution_bound(
  p_tenant_id uuid,
  p_contribution_id bigint,
  p_side text,
  out bound_at timestamptz,
  out bound_precision gnomon.time_precision
)
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select coalesce(s.bound_at, case p_side when 'from' then c.valid_from else c.valid_to end),
           coalesce(s.bound_precision,
                    case p_side when 'from' then c.valid_from_precision else c.valid_to_precision end)
    from gnomon.contribution c
    join gnomon.episode ce on ce.tenant_id = c.tenant_id and ce.id = c.episode_id
    cross join lateral (
      select gnomon.kind_merge_rule(
               c.tenant_id, gnomon.contribution_kind(c.tenant_id, c.node_id, c.edge_id)
             ) = 'ranked' as ranked
    ) r
    left join lateral (
      select x.bound_at, x.bound_precision from gnomon.bound_statement x
      join gnomon.episode se on se.tenant_id = x.tenant_id and se.id = x.episode_id
      where x.tenant_id = c.tenant_id and x.contribution_id = c.id and x.side = p_side
        and (not r.ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
      order by case when r.ranked then coalesce(se.authority, 0) else 0 end desc,
               case when r.ranked then x.episode_reference_time end desc nulls last,
               case when r.ranked then x.recorded_at end desc nulls last,
               x.id desc
      limit 1
    ) s on true
    where c.tenant_id = p_tenant_id and c.id = p_contribution_id
  $$;

-- The write path's recompute: it merges at the
-- start of its transaction under the rule the Kind has now, and the merge
-- locks the Node or Edge and refuses a write that a later transaction
-- overtook.
create function gnomon.recompute_versions(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_from timestamptz,
  p_to timestamptz,
  out changed boolean,
  out opened uuid[]
)
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_scope text;
    v_kind text;
    step record;
  begin
    select l.scope, l.kind into v_scope, v_kind
      from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select * into step from gnomon.merge_versions(
      p_tenant_id, v_scope, p_id, p_is_edge, p_from, p_to, transaction_timestamp(),
      gnomon.kind_merge_rule(p_tenant_id, v_kind), false);
    changed := step.changed;
    opened := step.opened;
  end
  $$;

-- Creates an Edge and returns its ID. For a
-- symmetric Kind it returns the Scope's Edge of that Kind between the two
-- Nodes when one exists, in either order, so creating the Edge from B to A
-- after the one from A to B addresses the same Edge. The insert trigger
-- checks the Kind and the endpoints first, in both cases.
create function gnomon.create_edge(
  p_tenant_id uuid,
  p_scope text,
  p_kind text,
  p_source_id uuid,
  p_target_id uuid
) returns uuid
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_id uuid;
  begin
    insert into gnomon.edge (tenant_id, scope, kind, source_id, target_id)
    values (p_tenant_id, p_scope, p_kind, p_source_id, p_target_id)
    on conflict (tenant_id, scope, kind, source_id, target_id)
      where gnomon.is_symmetric_kind(kind)
      do nothing
    returning id into v_id;
    if v_id is null then
      select e.id into v_id
        from gnomon.edge e
        where e.tenant_id = p_tenant_id and e.scope = p_scope and e.kind = p_kind
          and e.source_id = least(p_source_id, p_target_id)
          and e.target_id = greatest(p_source_id, p_target_id);
    end if;
    return v_id;
  end
  $$;

-- States a bound of a current Version's Valid time, citing one Episode, for
-- a Node or an Edge. `state_bound` and `state_edge_bound` call it. For an
-- Edge the statement is about the Contributions of `holds` too,
-- so the Edge holds over the stated bound.
create function gnomon.state_bound_of(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_precision gnomon.time_precision;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v gnomon.version;
    v_at timestamptz;
    v_earlier_at timestamptz;
    v_earlier_precision gnomon.time_precision;
    v_contributions bigint[];
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every statement cites an Episode';
    end if;
    if p_side is null or p_side not in ('from', 'to') then
      raise exception using errcode = 'GNW01', message = 'a bound is the from or the to bound';
    end if;
    if p_bound_at is null or p_bound_at in ('-infinity', 'infinity')
      or p_precision is null or p_precision not in ('year', 'month', 'day', 'instant')
    then
      raise exception using errcode = 'GNW01',
        message = 'a statement gives an instant and a precision of year, month, day or instant';
    end if;
    v_precision := p_precision::gnomon.time_precision;
    v_at := gnomon.period_start(p_bound_at, v_precision);

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;
    select * into v from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
        and x.id = p_version_id and x.recorded_to = 'infinity';
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Version %s is not a current Version of %s %s', p_version_id, v_of, p_id);
    end if;

    v_earlier_at := case p_side when 'from' then v.valid_from else v.valid_to end;
    v_earlier_precision := case p_side
      when 'from' then v.valid_from_precision else v.valid_to_precision end;
    change := gnomon.classify_bound(v_earlier_at, v_earlier_precision, v_at, v_precision);
    recorded_at := t;
    versions := '{}';
    if change = 'unchanged' then
      return next;
      return;
    end if;
    if (p_side = 'from' and v_at >= v.valid_to) or (p_side = 'to' and v.valid_from >= v_at) then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty',
          case p_side when 'from' then v_at else v.valid_from end,
          case p_side when 'from' then v.valid_to else v_at end);
    end if;

    -- The Contributions that gave the Version this bound.
    select coalesce(array_agg(c.id order by c.id), '{}') into v_contributions
      from gnomon.contribution c
      cross join lateral gnomon.contribution_bound(c.tenant_id, c.id, p_side) b
      cross join lateral gnomon.contribution_bound(
        c.tenant_id, c.id, case p_side when 'from' then 'to' else 'from' end) o
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
        and b.bound_at = v_earlier_at
        and case p_side
              when 'from' then o.bound_at > v.valid_from
              else o.bound_at < v.valid_to
            end
        and not (b.bound_at = v_at and b.bound_precision = v_precision);
    if cardinality(v_contributions) = 0 then
      raise exception using errcode = 'GNW01',
        message = format('no Contribution gave Version %s its %s bound', p_version_id, p_side);
    end if;

    insert into gnomon.bound_statement (
      tenant_id, scope, node_id, edge_id, episode_id, contribution_id, side, bound_at,
      bound_precision, episode_reference_time, episode_recorded_at
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, c, p_side, v_at, v_precision, v_reference_time, v_recorded_at
    from unnest(v_contributions) c;

    select * into step from gnomon.recompute_versions(
      p_tenant_id, p_id, p_is_edge, least(v.valid_from, v_at), greatest(v.valid_to, v_at));
    versions := step.opened;
    return next;
  end
  $$;

-- States a bound of a Node's current Version.
create function gnomon.state_bound(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select s.change, s.recorded_at, s.versions
    from gnomon.state_bound_of(
      p_tenant_id, p_node_id, false, p_episode_id, p_version_id, p_side, p_bound_at, p_precision) s
  $$;

-- States a bound of an Edge's current Version.
create function gnomon.state_edge_bound(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select s.change, s.recorded_at, s.versions
    from gnomon.state_bound_of(
      p_tenant_id, p_edge_id, true, p_episode_id, p_version_id, p_side, p_bound_at, p_precision) s
  $$;

-- Whether an As-of read at `p_valid_at` marks a Version `uncertain`: the
-- instant falls inside the period of an imprecise start bound (ADR-0008).
create function gnomon.uncertain_at(
  p_valid_at timestamptz,
  p_valid_from timestamptz,
  p_valid_from_precision gnomon.time_precision
) returns boolean
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select p_valid_from_precision in ('year', 'month', 'day')
      and p_valid_at < gnomon.period_end(p_valid_from, p_valid_from_precision)
  $$;

-- Reads an Edge As-of a valid-at and a known-at instant, as `read_node`
-- reads a Node, with its Kind, its endpoints and its Sentence, or a null
-- Sentence until one is stored. A null known-at reads the current belief.
-- The read follows the rule of `traverse`: an Edge with an endpoint the
-- caller cannot see reads as no Edge, so a read never names a Node outside
-- the caller's Scopes.
create function gnomon.read_edge(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    edge_id uuid,
    scope text,
    kind text,
    source_id uuid,
    target_id uuid,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text,
    sentence text
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select v.id, v.edge_id, v.scope, e.kind, e.source_id, e.target_id, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule,
           gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
           v.text_hash, s.sentence
    from gnomon.version v
    join gnomon.edge e on e.tenant_id = v.tenant_id and e.id = v.edge_id
    join gnomon.node src on src.tenant_id = e.tenant_id and src.id = e.source_id
    join gnomon.node tgt on tgt.tenant_id = e.tenant_id and tgt.id = e.target_id
    left join gnomon.sentence s
      on s.tenant_id = v.tenant_id and s.scope = v.scope and s.version_id = v.id
    where v.tenant_id = p_tenant_id and v.edge_id = p_edge_id
      and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
  $$;

-- Reads many Nodes As-of one valid-at and one known-at instant, in one
-- call: for each Node in `p_node_ids` that the caller can see and that held
-- a Version then, that Version, as `read_node` returns it. The rows come in
-- the order the Nodes were given, each Node once.
create function gnomon.read_nodes(
  p_tenant_id uuid,
  p_node_ids uuid[],
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    node_id uuid,
    scope text,
    kind text,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select v.id, v.node_id, v.scope, n.kind, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule,
           gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
           v.text_hash
    from (
      select x.node_id, min(x.ord) as ord
      from unnest(p_node_ids) with ordinality as x(node_id, ord)
      group by x.node_id
    ) w
    join gnomon.node n on n.tenant_id = p_tenant_id and n.id = w.node_id
    join gnomon.version v on v.tenant_id = n.tenant_id and v.scope = n.scope and v.node_id = n.id
    where v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
    order by w.ord
  $$;

-- Traverses the Edges of a Node As-of a valid-at and a known-at instant,
-- following an Edge of a symmetric Kind
-- from either of its Nodes: `out` and `in` each find it from both ends, and
-- `both` finds it once, as `out`. Its source and target are the canonical
-- order, and the neighbour is the Node at the other end. Every other Edge is
-- followed in its own direction.
create function gnomon.traverse(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_direction text,
  p_kinds text[]
) returns table (
    direction text,
    id uuid,
    edge_id uuid,
    scope text,
    kind text,
    source_id uuid,
    target_id uuid,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text,
    sentence text,
    neighbour_id uuid,
    neighbour_scope text,
    neighbour_kind text,
    neighbour_version_id uuid,
    neighbour_props jsonb,
    neighbour_valid_from timestamptz,
    neighbour_valid_from_precision gnomon.time_precision,
    neighbour_valid_to timestamptz,
    neighbour_valid_to_precision gnomon.time_precision,
    neighbour_recorded_from timestamptz,
    neighbour_recorded_to timestamptz,
    neighbour_merge_rule gnomon.merge_rule,
    neighbour_uncertain boolean,
    neighbour_text_hash text
  )
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if p_direction is null or p_direction not in ('out', 'in', 'both') then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a traversal follows Edges out, in or both';
    end if;
    return query
      with origin as (
        select o.tenant_id, o.id from gnomon.node o
        where o.tenant_id = p_tenant_id and o.id = p_node_id
      ), steps as (
        -- An Edge of a symmetric Kind is followed whichever end the Node is,
        -- in the direction asked for, and once, as `out`, for `both`. It
        -- never joins a Node to itself.
        select case when p_direction = 'in' and gnomon.is_symmetric_kind(e.kind)
                    then 'in' else 'out' end as direction,
               e.tenant_id, e.scope, e.id, e.kind,
               e.source_id, e.target_id, e.target_id as neighbour
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.source_id = o.id
        where p_direction in ('out', 'both') or gnomon.is_symmetric_kind(e.kind)
        union all
        select case when p_direction <> 'in' and gnomon.is_symmetric_kind(e.kind)
                    then 'out' else 'in' end,
               e.tenant_id, e.scope, e.id, e.kind, e.source_id, e.target_id, e.source_id
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.target_id = o.id
        where (p_direction in ('in', 'both') or gnomon.is_symmetric_kind(e.kind))
          and not (p_direction = 'both' and e.source_id = e.target_id)
      )
      select s.direction, v.id, s.id, s.scope, s.kind, s.source_id, s.target_id, v.props,
             v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
             v.recorded_from, v.recorded_to, v.merge_rule,
             gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
             v.text_hash, st.sentence,
             nb.id, nb.scope, nb.kind, nv.id, nv.props,
             nv.valid_from, nv.valid_from_precision, nv.valid_to, nv.valid_to_precision,
             nv.recorded_from, nv.recorded_to, nv.merge_rule,
             case when nv.id is not null
                  then gnomon.uncertain_at(p_valid_at, nv.valid_from, nv.valid_from_precision) end,
             nv.text_hash
      from steps s
      join gnomon.node nb on nb.tenant_id = s.tenant_id and nb.id = s.neighbour
      join gnomon.version v
        on v.tenant_id = s.tenant_id and v.scope = s.scope and v.edge_id = s.id
       and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
       and case
             when p_known_at is null then v.recorded_to = 'infinity'
             else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
           end
      left join gnomon.sentence st
        on st.tenant_id = v.tenant_id and st.scope = v.scope and st.version_id = v.id
      left join lateral (
        select x.* from gnomon.version x
        where x.tenant_id = nb.tenant_id and x.scope = nb.scope and x.node_id = nb.id
          and x.valid_from <= p_valid_at and x.valid_to > p_valid_at
          and case
                when p_known_at is null then x.recorded_to = 'infinity'
                else x.recorded_from <= p_known_at and x.recorded_to > p_known_at
              end
        limit 1
      ) nv on true
      where p_kinds is null or cardinality(p_kinds) = 0
         or gnomon.kind_lineage(s.tenant_id, s.kind) && p_kinds
      order by s.id, s.direction desc;
  end
  $$;

-- A statement is recorded when its transaction started, and serialises with
-- writes and Erasure on its Node or Edge (ADR-0007).
create function gnomon.statement_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a statement''s Recorded time is the start of its write''s transaction';
    end if;
    if num_nonnulls(new.node_id, new.edge_id) = 1 then
      perform pg_catalog.pg_advisory_xact_lock(
        gnomon.node_lock_key(new.tenant_id, coalesce(new.node_id, new.edge_id)));
    end if;
    return new;
  end
  $$;

create trigger statement_check_insert before insert on gnomon.statement
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.statement_check_insert();

-- Writes a patch of property values for a Node or an Edge, and records it as a Correction when `p_kind` is
-- 'correction': a statement row, and the Contributions name it. A Correction
-- needs a current Version over the Valid time it states, because it
-- replaces what that Version said. `write_patch` is now this function with no
-- kind.
create function gnomon.write_patch_as(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb,
  p_kind text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_values jsonb := p_values;
    vfp gnomon.time_precision;
    vtp gnomon.time_precision;
    vf timestamptz;
    vt timestamptz;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v_statement bigint;
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every write cites an Episode';
    end if;
    if p_valid_from in ('-infinity', 'infinity') or p_valid_to in ('-infinity', 'infinity') then
      raise exception using errcode = 'GNW01',
        message = 'a stated Valid-time bound is an instant; leave an unknown bound null';
    end if;
    if (p_valid_from is null and p_valid_from_precision is distinct from 'unknown'
          and p_valid_from_precision is not null)
      or (p_valid_to is null and p_valid_to_precision is distinct from 'unknown'
          and p_valid_to_precision is not null)
      or p_valid_from is not null and p_valid_from_precision = 'unknown'
      or p_valid_to is not null and p_valid_to_precision = 'unknown'
    then
      raise exception using errcode = 'GNW01',
        message = 'a stated bound has a precision of year, month, day or instant, and an unknown bound has none';
    end if;
    if p_valid_from_precision is not null
      and p_valid_from_precision not in ('unknown', 'year', 'month', 'day', 'instant')
      or p_valid_to_precision is not null
      and p_valid_to_precision not in ('unknown', 'year', 'month', 'day', 'instant')
    then
      raise exception using errcode = 'GNW01',
        message = 'a precision is year, month, day or instant';
    end if;
    vfp := coalesce(p_valid_from_precision, case when p_valid_from is null then 'unknown' else 'instant' end)
      ::gnomon.time_precision;
    vtp := coalesce(p_valid_to_precision, case when p_valid_to is null then 'unknown' else 'instant' end)
      ::gnomon.time_precision;
    vf := coalesce(gnomon.period_start(p_valid_from, vfp), '-infinity');
    vt := coalesce(gnomon.period_start(p_valid_to, vtp), 'infinity');
    if vf >= vt then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty', vf, vt);
    end if;
    if p_is_edge then
      v_values := coalesce(v_values, '{}');
      if jsonb_typeof(v_values) <> 'object' then
        raise exception using errcode = 'GNW01',
          message = 'a write states its values as {property: value}';
      end if;
      if exists (select from gnomon.edge_builtin_rules() b where v_values ? b.property) then
        raise exception using errcode = 'GNW01',
          message = '"holds" is built in: every write to an Edge states that the Edge holds over its Valid time';
      end if;
    elsif jsonb_typeof(v_values) is distinct from 'object' or v_values = '{}' then
      raise exception using errcode = 'GNW01',
        message = 'a write states a value for at least one property';
    end if;
    if p_places is not null and (
      jsonb_typeof(p_places) <> 'object'
      or exists (
        select from jsonb_each(p_places) p
        where jsonb_typeof(p.value) <> 'string' or not v_values ? p.key
      )
    ) then
      raise exception using errcode = 'GNW01',
        message = 'places are {property: place}, a string for a property the write states';
    end if;

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;

    if p_kind = 'correction' then
      if not exists (
        select from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.recorded_to = 'infinity' and x.valid_from < vt and x.valid_to > vf
      ) then
        raise exception using errcode = 'GNW01',
          message = format('%s %s has no current Version over [%s, %s) to correct', v_of, p_id, vf, vt);
      end if;
      insert into gnomon.statement (
        tenant_id, scope, node_id, edge_id, episode_id, kind,
        valid_from, valid_from_precision, valid_to, valid_to_precision,
        episode_reference_time, episode_recorded_at
      ) values (
        p_tenant_id, v_scope, case when not p_is_edge then p_id end,
        case when p_is_edge then p_id end, p_episode_id, 'correction',
        vf, vfp, vt, vtp, v_reference_time, v_recorded_at
      ) returning id into v_statement;
    end if;

    -- In property order, so the first refused value is the same on every
    -- host.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, edge_id, episode_id, property, value, place,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, statement_id
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, x.key, x.value, p_places ->> x.key,
           vf, vfp, vt, vtp, v_reference_time, v_recorded_at, v_statement
    from (
      select e.key, e.value from jsonb_each(v_values) e
      union all
      select b.property, 'true'::jsonb from gnomon.edge_builtin_rules() b where p_is_edge
    ) x
    order by x.key;

    select * into step from gnomon.recompute_versions(p_tenant_id, p_id, p_is_edge, vf, vt);
    status := case when step.changed then 'written' else 'unchanged' end;
    recorded_at := transaction_timestamp();
    versions := step.opened;
    return next;
  end
  $$;

create function gnomon.write_patch(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_id, p_is_edge, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, null) w
  $$;

-- Writes a patch of property values for an Edge, citing one Episode, over
-- one Valid time, and records that the Edge holds then. `p_values` may be
-- null or empty. The rest is as `gnomon.write_node`.
create function gnomon.write_edge(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch(
      p_tenant_id, p_edge_id, true, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places) w
  $$;

-- Writes a patch of property values for a Node.
create function gnomon.write_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch(
      p_tenant_id, p_node_id, false, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places) w
  $$;

-- Records a Correction of a Node: as `gnomon.write_node`, and the Node must
-- have a current Version over the Valid time.
create function gnomon.correct_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_node_id, false, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, 'correction') w
  $$;

-- Records a Correction of an Edge, as `gnomon.write_edge` does for a write.
create function gnomon.correct_edge(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_edge_id, true, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, 'correction') w
  $$;

-- Lists the statements about a Node or an Edge, in the order they were
-- recorded, with the Episode that made each. A statement that lost on
-- Authority is listed too: it is held.
create function gnomon.list_statements(p_tenant_id uuid, p_id uuid)
  returns table (
    kind text,
    episode_id uuid,
    version_id uuid,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_at timestamptz
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select s.kind, s.episode_id, s.version_id, s.valid_from, s.valid_from_precision,
           s.valid_to, s.valid_to_precision, s.recorded_at
    from gnomon.statement s
    where s.tenant_id = p_tenant_id and s.versioned_id = p_id
    order by s.id
  $$;

-- The marker's Scope is its Episode's, and the caller names only the Episode.
-- The Episode must be visible to the caller.
create function gnomon.episode_retraction_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'an Episode''s Retraction is recorded when its transaction started';
    end if;
    select e.scope into new.scope
      from gnomon.episode e where e.tenant_id = new.tenant_id and e.id = new.episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible to the caller', new.episode_id);
    end if;
    return new;
  end
  $$;

create trigger episode_retraction_check_insert before insert on gnomon.episode_retraction
  for each row execute function gnomon.episode_retraction_check_insert();

-- At commit, a retracted Episode has no current Version that only it cites,
-- and a statement that retracts an Episode belongs to an Episode retracted in
-- the same transaction. A row inserted directly, without Retract Episode's
-- Retractions, fails here.
create function gnomon.episode_retraction_check_complete() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if tg_table_name = 'episode_retraction' then
      if exists (
        select from gnomon.version x
        where x.tenant_id = new.tenant_id and x.recorded_to = 'infinity'
          and exists (select from gnomon.citation c
                      where c.tenant_id = x.tenant_id and c.version_id = x.id
                        and c.episode_id = new.episode_id)
          and not exists (select from gnomon.citation c
                          where c.tenant_id = x.tenant_id and c.version_id = x.id
                            and c.episode_id <> new.episode_id)
      ) then
        raise exception using errcode = 'GNW01',
          message = format('Episode %s still has a current Version that only it cites; '
            'retract it with gnomon.retract_episode', new.episode_id);
      end if;
    elsif new.retracts_episode and not exists (
      select from gnomon.episode_retraction r
      where r.tenant_id = new.tenant_id and r.episode_id = new.episode_id
        and r.recorded_at = new.recorded_at
    ) then
      raise exception using errcode = 'GNW01',
        message = 'only Retract Episode makes a Retraction that retracts its Episode';
    end if;
    return null;
  end
  $$;

create constraint trigger episode_retraction_complete
  after insert on gnomon.episode_retraction
  deferrable initially deferred
  for each row execute function gnomon.episode_retraction_check_complete();

create constraint trigger statement_retracts_episode
  after insert on gnomon.statement
  deferrable initially deferred
  for each row when (new.retracts_episode)
  execute function gnomon.episode_retraction_check_complete();

-- An End of validity or a Retraction, with the flag that marks a Retraction
-- made by Retract Episode. `write_ending` is this function with no flag.
create function gnomon.write_ending_as(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_kind text,
  p_version_id uuid,
  p_at timestamptz,
  p_precision text,
  p_retracts_episode boolean
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_precision gnomon.time_precision;
    v_at timestamptz;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v gnomon.version;
    sf timestamptz;
    sfp gnomon.time_precision;
    v_statement bigint;
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every statement cites an Episode';
    end if;
    if p_kind = 'end_of_validity' then
      if p_at is null or p_at in ('-infinity', 'infinity')
        or p_precision is null or p_precision not in ('year', 'month', 'day', 'instant')
      then
        raise exception using errcode = 'GNW01',
          message = 'an End of validity gives an instant and a precision of year, month, day or instant';
      end if;
      v_precision := p_precision::gnomon.time_precision;
      v_at := gnomon.period_start(p_at, v_precision);
    elsif p_version_id is null then
      raise exception using errcode = 'GNW01',
        message = 'a Retraction names the Version it retracts';
    end if;

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;

    if p_kind = 'retraction' then
      select * into v from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.id = p_version_id and x.recorded_to = 'infinity';
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('Version %s is not a current Version of %s %s', p_version_id, v_of, p_id);
      end if;
      sf := v.valid_from;
      sfp := v.valid_from_precision;
    else
      select * into v from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.recorded_to = 'infinity' and x.valid_from < v_at and x.valid_to > v_at;
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('%s %s has no current Version that holds across %s; '
            'a Retraction states that a Version never held', v_of, p_id, v_at);
      end if;
      sf := v_at;
      sfp := v_precision;
    end if;

    insert into gnomon.statement (
      tenant_id, scope, node_id, edge_id, episode_id, kind, version_id,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, retracts_episode
    ) values (
      p_tenant_id, v_scope, case when not p_is_edge then p_id end,
      case when p_is_edge then p_id end, p_episode_id, p_kind, v.id,
      sf, sfp, v.valid_to, v.valid_to_precision, v_reference_time, v_recorded_at,
      coalesce(p_retracts_episode, false)
    ) returning id into v_statement;

    -- In property order, as a write's Contributions are.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, edge_id, episode_id, property, value,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, statement_id
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, x.property, 'null'::jsonb,
           sf, sfp, v.valid_to, v.valid_to_precision, v_reference_time, v_recorded_at, v_statement
    from (
      select k as property from jsonb_object_keys(v.props) k
        where not p_is_edge or p_kind = 'retraction'
      union
      select b.property from gnomon.edge_builtin_rules() b where p_is_edge
    ) x
    order by x.property;

    select * into step from gnomon.recompute_versions(
      p_tenant_id, p_id, p_is_edge, sf, v.valid_to);
    status := case when step.changed then 'written' else 'unchanged' end;
    recorded_at := transaction_timestamp();
    versions := step.opened;
    return next;
  end
  $$;

create function gnomon.write_ending(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_kind text,
  p_version_id uuid,
  p_at timestamptz,
  p_precision text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending_as(
      p_tenant_id, p_id, p_is_edge, p_episode_id, p_kind, p_version_id, p_at, p_precision, false) w
  $$;

create function gnomon.retract_version(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_version_id uuid
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending(
      p_tenant_id, p_id, p_is_edge, p_episode_id, 'retraction', p_version_id, null, null) w
  $$;

create function gnomon.end_validity(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_at timestamptz,
  p_precision text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending(
      p_tenant_id, p_id, p_is_edge, p_episode_id, 'end_of_validity', null, p_at, p_precision) w
  $$;

-- Retract Episode: records a Retraction for each current Version cited only
-- by the Episode, and stops the Episode's own statements counting. It
-- returns the Versions it retracted. A second call for the same Episode
-- retracts nothing.
--
-- It takes the lock of every Node and Edge it touches up front, in a fixed
-- order, so two Retract Episodes cannot deadlock and a write waits for it
-- (ADR-0007).
create function gnomon.retract_episode(p_tenant_id uuid, p_episode_id uuid)
  returns table (recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_scope text;
    t record;
    v record;
    v_done uuid[] := '{}';
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'Retract Episode may act only in the caller''s own Tenant';
    end if;
    select e.scope into v_scope
      from gnomon.episode e where e.tenant_id = p_tenant_id and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible to the caller', p_episode_id);
    end if;
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_episode_id));
    recorded_at := transaction_timestamp();
    if exists (select from gnomon.episode_retraction r
               where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id) then
      versions := '{}';
      return next;
      return;
    end if;

    -- Every Node and Edge the Episode touched, locked in id order.
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, x.id))
      from (
        select c.versioned_id as id from gnomon.contribution c
          where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
        union
        select s.versioned_id from gnomon.statement s
          where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
        union
        select b.versioned_id from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
      ) x
      order by x.id;

    -- The Retractions: each current Version that only this Episode cites.
    for v in
      select x.id, x.versioned_id, x.edge_id is not null as is_edge
      from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.recorded_to = 'infinity'
        and exists (select from gnomon.citation c
                    where c.tenant_id = x.tenant_id and c.version_id = x.id
                      and c.episode_id = p_episode_id)
        and not exists (select from gnomon.citation c
                        where c.tenant_id = x.tenant_id and c.version_id = x.id
                          and c.episode_id <> p_episode_id)
      order by x.versioned_id, x.valid_from, x.id
    loop
      -- An earlier Retraction may have recomputed a Version that touches this
      -- one, but a recompute keeps a Version whose values and Valid time have
      -- not changed. One that changed is no longer cited by this Episode alone.
      if exists (select from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.id = v.id and x.recorded_to = 'infinity') then
        perform gnomon.write_ending_as(
          p_tenant_id, v.versioned_id, v.is_edge, p_episode_id, 'retraction', v.id, null, null,
          true);
        -- Under ranked merge a Retraction can lose, and then the Version stays
        -- current: it is not retracted and is not reported.
        if not exists (select from gnomon.version x
                       where x.tenant_id = p_tenant_id and x.id = v.id
                         and x.recorded_to = 'infinity') then
          v_done := v_done || v.id;
        end if;
      end if;
    end loop;

    -- From here the Episode's other statements stop counting.
    insert into gnomon.episode_retraction (tenant_id, episode_id)
      values (p_tenant_id, p_episode_id);

    -- Recompute what those statements shaped, held or in effect.
    for t in
      select x.id, x.is_edge, min(x.vf) as vf, max(x.vt) as vt
      from (
        select s.versioned_id as id, s.edge_id is not null as is_edge,
               s.valid_from as vf, s.valid_to as vt
          from gnomon.statement s
          where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
            and not s.retracts_episode
        union all
        -- A bound statement can move a bound of the Contribution it names
        -- anywhere, so the whole history is recomputed.
        select b.versioned_id, b.edge_id is not null, '-infinity'::timestamptz, 'infinity'::timestamptz
          from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
      ) x
      group by x.id, x.is_edge
      order by x.id
    loop
      perform gnomon.recompute_versions(p_tenant_id, t.id, t.is_edge, t.vf, t.vt);
    end loop;

    versions := v_done;
    return next;
  end
  $$;

-- At commit, every Version a caller inserted that still stands has a
-- Citation, so no caller can commit a Version that no Episode cites: Erase
-- Episode starts from an Episode's Citations and would never find it. The
-- write path inserts each Version's Citations in the same transaction, so
-- this finds one at once. A Version the transaction replaced is gone and
-- needs none. Which rows the check sees depends on the claims at commit, so
-- a caller whose claims no longer reach the Version's Tenant and Scope is
-- refused rather than let through. The trigger's condition is evaluated at
-- insert, so a Version Erasure writes as the definer is never queued, and
-- Erasure may delete Citations as its replay requires.
create function gnomon.version_check_cited() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if exists (select from gnomon.citation c
               where c.tenant_id = new.tenant_id and c.scope = new.scope
                 and c.version_id = new.id) then
      return null;
    end if;
    if new.tenant_id is distinct from gnomon.tenant_id()
       or not new.scope = any(gnomon.allowed_scopes())
       or exists (select from gnomon.version v
                  where v.tenant_id = new.tenant_id and v.id = new.id) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Version %s cites no Episode', new.id);
    end if;
    return null;
  end
  $$;

create constraint trigger version_check_cited after insert on gnomon.version
  deferrable initially deferred
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_check_cited();

-- Creating an Edge takes the locks of both its endpoints, in order of their
-- IDs, as an Erasure does (see 0005). The trigger's name sorts before
-- `edge_check_insert`, so the check runs after the lock and sees an endpoint
-- an Erasure erased as gone.
create function gnomon.edge_await_endpoints() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    x uuid;
  begin
    for x in select distinct e.id from unnest(array[new.source_id, new.target_id]) e(id)
             where e.id is not null order by e.id
    loop
      perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(new.tenant_id, x));
    end loop;
    return new;
  end
  $$;

create trigger edge_await_endpoints before insert on gnomon.edge
  for each row execute function gnomon.edge_await_endpoints();

-- The merge, at a chosen Recorded time and under a chosen
-- rule, so that the write path and Erasure share it (ADR-0007). It reads only
-- the Contributions, bound statements and Episode Retractions recorded by
-- `p_at`, and recomputes the open Versions of a Node or an Edge over a
-- Valid-time window:
--
--   p_at      the step's Recorded time. The write path passes the start of
--             its transaction; an Erasure passes the step it replays.
--   p_rule    `recency` or `ranked`. The Versions the step opens record it.
--   p_replay  false for the write path. A Version or Citation the current
--             step wrote is one its transaction wrote, and a Version the step
--             closed is one its transaction closed, as before.
--             true for an Erasure's replay, which runs as `gnomon_definer`
--             past the guards: the step wrote what was recorded at `p_at`
--             and closed what it closed at `p_at`, and what it writes is
--             recorded at `p_at` and marked written by no transaction.
--
-- A write takes the Node's or Edge's lock here, and is refused when a
-- transaction that started later wrote the Node or Edge first, so a caller
-- that calls the merge directly gets the write path's guarantees. An Erasure
-- has taken its locks already.
create function gnomon.merge_versions(
  p_tenant_id uuid,
  p_scope text,
  p_id uuid,
  p_is_edge boolean,
  p_from timestamptz,
  p_to timestamptz,
  p_at timestamptz,
  p_rule text,
  p_replay boolean,
  out changed boolean,
  out opened uuid[]
)
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    t constant timestamptz := p_at;
    me constant xid8 := pg_current_xact_id();
    v_scope constant text := p_scope;
    v_ranked constant boolean := p_rule = 'ranked';
    w0 timestamptz;
    w1 timestamptz;
    candidates jsonb;
    v_fresh jsonb;
    stale uuid[];
    -- The statements and bound statements of this Node or Edge that belong to
    -- an Episode retracted by `p_at`, which the merge leaves out.
    ignored_statements bigint[];
    ignored_bounds bigint[];
  begin
    if p_rule is null or p_rule not in ('recency', 'ranked') then
      raise exception using errcode = 'GNW01', message = 'a merge rule is recency or ranked';
    end if;
    if p_replay and not gnomon.running_as_definer() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'only Erasure replays history';
    end if;
    if not p_replay then
      if p_at is distinct from transaction_timestamp() then
        raise exception using errcode = 'insufficient_privilege',
          message = 'a write merges at the start of its own transaction';
      end if;
      if (select l.scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l)
           is distinct from p_scope then
        raise exception using errcode = 'GNW01',
          message = format('%s %s is not in Scope %s',
                           case when p_is_edge then 'Edge' else 'Node' end, p_id, p_scope);
      end if;
      -- A transaction that started before another wrote this Node or Edge,
      -- but took the lock after that one committed, would record a Version
      -- earlier than one already recorded. It fails with a serialization
      -- failure, and the caller retries the transaction.
      if exists (
        select from gnomon.contribution c
        where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
          and c.recorded_at > t
      ) or exists (
        select from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.recorded_from = t and x.written_in <> me
      ) then
        raise exception using errcode = 'serialization_failure',
          message = format('a later transaction wrote %s %s first; retry the write',
                           case when p_is_edge then 'Edge' else 'Node' end, p_id);
      end if;
    end if;
    select coalesce(array_agg(st.id), '{}') into ignored_statements
      from gnomon.statement st
      where st.tenant_id = p_tenant_id and st.scope = v_scope and st.versioned_id = p_id
        and not st.retracts_episode and st.recorded_at <= t
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = st.tenant_id and r.episode_id = st.episode_id
                      and r.recorded_at <= t);
    select coalesce(array_agg(b.id), '{}') into ignored_bounds
      from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.scope = v_scope and b.versioned_id = p_id
        and b.recorded_at <= t
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = b.tenant_id and r.episode_id = b.episode_id
                      and r.recorded_at <= t);

    -- 1. The window.
    select least(p_from, min(v.valid_from)), greatest(p_to, max(v.valid_to)) into w0, w1
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
        and v.recorded_to = 'infinity'
        and v.valid_from <= p_to and v.valid_to >= p_from;

    -- 2 and 3. The new Versions over the window, each with its Citations.
    with contributions as (
      select c.id, c.episode_id, c.property, c.value, c.place,
             greatest(e.vf, w0) as vf, least(e.vt, w1) as vt,
             case when e.vf >= w0 then e.vfp end as vfp,
             case when e.vt <= w1 then e.vtp end as vtp,
             case when v_ranked then coalesce(ce.authority, 0) else 0 end as auth,
             c.episode_reference_time as ref, c.episode_recorded_at as episode_rec,
             c.recorded_at as rec
      from gnomon.contribution c
      join gnomon.episode ce on ce.tenant_id = c.tenant_id and ce.id = c.episode_id
      cross join lateral (
        select coalesce(f.bound_at, c.valid_from) as vf,
               coalesce(f.bound_precision, c.valid_from_precision) as vfp,
               coalesce(u.bound_at, c.valid_to) as vt,
               coalesce(u.bound_precision, c.valid_to_precision) as vtp
        from (select 1) one
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          join gnomon.episode se on se.tenant_id = s.tenant_id and se.id = s.episode_id
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'from'
            and s.recorded_at <= t
            and s.id <> all (ignored_bounds)
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
        ) f on true
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          join gnomon.episode se on se.tenant_id = s.tenant_id and se.id = s.episode_id
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'to'
            and s.recorded_at <= t
            and s.id <> all (ignored_bounds)
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
        ) u on true
      ) e
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
        and c.recorded_at <= t
        and e.vf < w1 and e.vt > w0
        -- The statements of a retracted Episode no longer count, except the
        -- Retractions Retract Episode made.
        and (c.statement_id is null or c.statement_id <> all (ignored_statements))
    ), bounds as (
      select vf as at from contributions
      union select vt from contributions
      union select w0
      union select w1
    ), segments as (
      select at as sf, lead(at) over (order by at) as st from bounds
    ), winners as (
      select distinct on (s.sf, c.property) s.sf, s.st, c.property, c.value
      from segments s join contributions c on c.vf <= s.sf and c.vt >= s.st
      where s.st is not null
      order by s.sf, c.property, c.auth desc, c.ref desc, c.rec desc, c.id desc
    ), supporters as (
      -- Every Contribution in a segment that states its winning value, and
      -- whether the step recorded it.
      select w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec,
             bool_or(c.rec = t) as fresh
      from winners w
      join contributions c
        on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
      where w.value <> 'null'::jsonb
      group by w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec
    ), segment_values as (
      select sf, max(st) as st,
             coalesce(
               jsonb_object_agg(property, value)
                 filter (where value <> 'null'::jsonb and not (p_is_edge and property = 'holds')),
               '{}'::jsonb) as props,
             case when p_is_edge then coalesce(bool_or(property = 'holds' and value <> 'null'::jsonb), false)
                  else bool_or(value <> 'null'::jsonb) end as held
      from winners group by sf
    ), islands as (
      select sf, st, props,
             sum(case when prev_st = sf and prev_props = props then 0 else 1 end)
               over (order by sf) as island
      from (
        select sf, st, props,
               lag(st) over (order by sf) as prev_st, lag(props) over (order by sf) as prev_props
        from segment_values where held
      ) x
    ), merged as (
      select island, min(sf) as vf, max(st) as vt, (array_agg(props))[1] as props
      from islands group by island
    ), merged_precision as (
      select m.island, m.vf, m.vt, m.props,
             coalesce(
               (select max(c.vfp) from contributions c where c.vf = m.vf),
               (select x.valid_from_precision from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
                   and x.recorded_to = 'infinity' and x.valid_from = m.vf limit 1),
               gnomon.instant_or_unknown(m.vf)) as vfp,
             coalesce(
               (select max(c.vtp) from contributions c where c.vt = m.vt),
               (select x.valid_to_precision from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
                   and x.recorded_to = 'infinity' and x.valid_to = m.vt limit 1),
               gnomon.instant_or_unknown(m.vt)) as vtp
      from merged m
    ), island_citations as (
      select i.island,
             jsonb_agg(distinct jsonb_build_array(
               s.property, s.episode_id, s.place, s.fresh, s.ref, s.episode_rec)) as cites
      from islands i join supporters s on s.sf = i.sf
      group by i.island
    )
    select coalesce(jsonb_agg(jsonb_build_object(
             'vf', m.vf, 'vt', m.vt, 'vfp', m.vfp, 'vtp', m.vtp, 'props', m.props,
             'cites', coalesce(ic.cites, '[]'::jsonb))),
           '[]'::jsonb)
      into candidates
      from merged_precision m left join island_citations ic using (island);

    -- 4. The stale Versions. One the step wrote is deleted with its
    -- Citations, so no empty Recorded time is left; every other is closed at
    -- the step and loses the Citations the step added to it.
    select coalesce(array_agg(v.id), '{}') into stale
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
        and v.recorded_to = 'infinity'
        and v.valid_from >= w0 and v.valid_to <= w1
        and not exists (
          select from jsonb_to_recordset(candidates)
            cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
          where cand.vf = v.valid_from and cand.vt = v.valid_to and cand.props = v.props
            and cand.vfp = v.valid_from_precision::text and cand.vtp = v.valid_to_precision::text
        );
    changed := cardinality(stale) > 0;
    delete from gnomon.citation x
      where x.tenant_id = p_tenant_id and x.version_id = any(stale)
        and case when p_replay then x.recorded_at = t else x.written_in = me end;
    delete from gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(stale)
        and case when p_replay then v.recorded_from = t else v.written_in = me end;
    update gnomon.version v set recorded_to = t
      where v.tenant_id = p_tenant_id and v.id = any(stale);

    -- 5. Reopen what the step closed and now restores, and insert the rest.
    -- The new Versions are those no open Version, and none the step closed,
    -- already holds, found before the reopening.
    select coalesce(jsonb_agg(cand.c), '[]') into v_fresh
      from jsonb_array_elements(candidates) cand(c)
      where not exists (
        select from gnomon.version v
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.valid_from = (cand.c ->> 'vf')::timestamptz
          and v.valid_to = (cand.c ->> 'vt')::timestamptz
          and v.props = cand.c -> 'props'
          and v.valid_from_precision::text = cand.c ->> 'vfp'
          and v.valid_to_precision::text = cand.c ->> 'vtp'
          and (v.recorded_to = 'infinity'
               or (v.recorded_to = t and (p_replay or v.closed_in = me)))
      );
    with reopened as (
      update gnomon.version v set recorded_to = 'infinity'
        from jsonb_to_recordset(candidates)
          cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.recorded_to = t and (p_replay or v.closed_in = me)
          and v.valid_from = cand.vf and v.valid_to = cand.vt and v.props = cand.props
          and v.valid_from_precision::text = cand.vfp and v.valid_to_precision::text = cand.vtp
        returning v.id
    )
    select coalesce(array_agg(id), '{}') into opened from reopened;
    if p_replay then
      with inserted as (
        insert into gnomon.version (
          tenant_id, scope, node_id, edge_id, props, valid_from, valid_from_precision,
          valid_to, valid_to_precision, merge_rule, recorded_from, recorded_to, written_in
        )
        select p_tenant_id, v_scope,
               case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
               f.props, f.vf, f.vfp::gnomon.time_precision, f.vt, f.vtp::gnomon.time_precision,
               p_rule::gnomon.merge_rule, t, 'infinity', '0'::xid8
        from jsonb_to_recordset(v_fresh)
          f(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        returning id
      )
      select opened || coalesce(array_agg(id), '{}') into opened from inserted;
    else
      -- A caller may not name a Recorded time or a transaction mark, so the
      -- write path leaves them to the defaults.
      with inserted as (
        insert into gnomon.version (
          tenant_id, scope, node_id, edge_id, props, valid_from, valid_from_precision,
          valid_to, valid_to_precision, merge_rule
        )
        select p_tenant_id, v_scope,
               case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
               f.props, f.vf, f.vfp::gnomon.time_precision, f.vt, f.vtp::gnomon.time_precision,
               p_rule::gnomon.merge_rule
        from jsonb_to_recordset(v_fresh)
          f(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        returning id
      )
      select opened || coalesce(array_agg(id), '{}') into opened from inserted;
    end if;
    changed := changed or cardinality(opened) > 0;

    -- 6. The Citations.
    if p_replay then
      insert into gnomon.citation (
        tenant_id, scope, version_id, episode_id, property, place,
        episode_reference_time, episode_recorded_at, recorded_at, written_in
      )
        select p_tenant_id, v_scope, v.id, (cc ->> 1)::uuid, cc ->> 0, cc ->> 2,
               (cc ->> 4)::timestamptz, (cc ->> 5)::timestamptz, t, '0'::xid8
        from jsonb_to_recordset(candidates) cand(vf timestamptz, vt timestamptz, cites jsonb)
        join gnomon.version v
          on v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
         and v.recorded_to = 'infinity'
         and v.valid_from = cand.vf and v.valid_to = cand.vt
        cross join lateral jsonb_array_elements(cand.cites) cc
        where v.id = any(opened) or (cc ->> 3)::boolean
        on conflict do nothing;
    else
      insert into gnomon.citation (
        tenant_id, scope, version_id, episode_id, property, place,
        episode_reference_time, episode_recorded_at
      )
        select p_tenant_id, v_scope, v.id, (cc ->> 1)::uuid, cc ->> 0, cc ->> 2,
               (cc ->> 4)::timestamptz, (cc ->> 5)::timestamptz
        from jsonb_to_recordset(candidates) cand(vf timestamptz, vt timestamptz, cites jsonb)
        join gnomon.version v
          on v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
         and v.recorded_to = 'infinity'
         and v.valid_from = cand.vf and v.valid_to = cand.vt
        cross join lateral jsonb_array_elements(cand.cites) cc
        where v.id = any(opened) or (cc ->> 3)::boolean
        on conflict do nothing;
    end if;
  end
  $$;

-- Writes a pending record of a stage of an Episode the caller sees, unless a
-- record of that stage exists already, which it leaves as it is.
create function gnomon.stage_pending(p_tenant_id uuid, p_episode_id uuid, p_stage text)
  returns void
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_scope text;
  begin
    select e.scope into v_scope
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.id = p_episode_id;
    if v_scope is null then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Episode %s is not visible to the caller', p_episode_id);
    end if;
    insert into gnomon.stage_record (tenant_id, scope, episode_id, stage)
      values (p_tenant_id, v_scope, p_episode_id, p_stage)
      on conflict (tenant_id, episode_id, stage) do nothing;
  end
  $$;

-- Claims one due record in the caller's Scopes, or of one Episode when
-- `p_episode_id` is not null: pending, retry-due, or running with an expired
-- lease, the one due longest first. It returns the claimed record, or no row.
create function gnomon.claim_stage(p_tenant_id uuid, p_episode_id uuid, p_lease interval)
  returns setof gnomon.stage_record
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if p_lease is null or p_lease < interval '0' then
      raise exception using errcode = 'GNW01', message = 'a lease is zero or longer';
    end if;
    return query
      update gnomon.stage_record r
        set status = 'running',
            attempts = r.attempts + 1,
            lease_until = pg_catalog.now() + p_lease,
            retry_at = null,
            updated_at = pg_catalog.now()
        where (r.tenant_id, r.episode_id, r.stage) = (
          select d.tenant_id, d.episode_id, d.stage
          from gnomon.stage_record d
          where d.tenant_id = p_tenant_id
            and (p_episode_id is null or d.episode_id = p_episode_id)
            and (d.status = 'pending'
                 or (d.status = 'retry' and d.retry_at <= pg_catalog.now())
                 or (d.status = 'running' and d.lease_until <= pg_catalog.now()))
          order by case d.status
                     when 'pending' then d.created_at
                     when 'retry' then d.retry_at
                     else d.lease_until
                   end,
                   d.episode_id, d.stage
          limit 1
          for update skip locked)
        returning r.*;
  end
  $$;

-- Finishes a claimed record as succeeded, retry or failed. `p_attempt` is the
-- attempt count its claim returned. A retry is due `p_retry_after` from now.
-- A success keeps the last error an earlier attempt recorded.
create function gnomon.finish_stage(
  p_tenant_id uuid,
  p_episode_id uuid,
  p_stage text,
  p_attempt integer,
  p_status gnomon.stage_status,
  p_error_code text,
  p_error_status integer,
  p_retry_after interval
) returns void
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if p_status is null or p_status not in ('succeeded', 'retry', 'failed') then
      raise exception using errcode = 'GNW01',
        message = 'a claimed stage finishes as succeeded, retry or failed';
    end if;
    if (p_status = 'succeeded') <> (p_error_code is null)
       or (p_error_code is null and p_error_status is not null) then
      raise exception using errcode = 'GNW01',
        message = 'a stage that fails records an error code, and one that succeeds records no error';
    end if;
    if (p_status = 'retry') <> (p_retry_after is not null) or p_retry_after < interval '0' then
      raise exception using errcode = 'GNW01',
        message = 'a retry, and only a retry, is due after an interval of zero or longer';
    end if;
    update gnomon.stage_record r
      set status = p_status,
          lease_until = null,
          retry_at = pg_catalog.now() + p_retry_after,
          last_error_code = coalesce(p_error_code, r.last_error_code),
          last_error_status = case when p_error_code is null then r.last_error_status
                                   else p_error_status end,
          updated_at = pg_catalog.now()
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id and r.stage = p_stage
        and r.status = 'running' and r.attempts = p_attempt;
    if not found then
      raise exception using errcode = 'GNS01',
        message = pg_catalog.format(
          'the claim of stage %s of Episode %s at attempt %s is no longer held',
          p_stage, p_episode_id, p_attempt);
    end if;
  end
  $$;

-- Adds a model's usage to a stage record of the caller's.
create function gnomon.add_stage_usage(
  p_tenant_id uuid,
  p_episode_id uuid,
  p_stage text,
  p_model text,
  p_calls integer,
  p_input_tokens bigint,
  p_output_tokens bigint,
  p_cost_usd numeric
) returns void
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    insert into gnomon.stage_usage as u
        (tenant_id, scope, episode_id, stage, model, calls, input_tokens, output_tokens, cost_usd)
      select r.tenant_id, r.scope, r.episode_id, r.stage, p_model, p_calls, p_input_tokens,
             p_output_tokens, p_cost_usd
      from gnomon.stage_record r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id and r.stage = p_stage
      on conflict (tenant_id, episode_id, stage, model) do update
        set calls = u.calls + excluded.calls,
            input_tokens = u.input_tokens + excluded.input_tokens,
            output_tokens = u.output_tokens + excluded.output_tokens,
            cost_usd = case when u.cost_usd is null and excluded.cost_usd is null then null
                            else coalesce(u.cost_usd, 0) + coalesce(excluded.cost_usd, 0) end;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'no stage record of stage %s of Episode %s is visible to the caller',
          p_stage, p_episode_id);
    end if;
  end
  $$;

-- Every stage record of an Episode the caller sees, in the order written.
create function gnomon.read_stage_records(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    tenant_id uuid,
    scope text,
    episode_id uuid,
    stage text,
    status gnomon.stage_status,
    attempts integer,
    last_error_code text,
    last_error_status integer,
    retry_at timestamptz,
    lease_until timestamptz,
    created_at timestamptz,
    updated_at timestamptz
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select r.tenant_id, r.scope, r.episode_id, r.stage, r.status, r.attempts,
           r.last_error_code, r.last_error_status,
           r.retry_at, r.lease_until, r.created_at, r.updated_at
    from gnomon.stage_record r
    where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id
    order by r.created_at, r.stage
  $$;

-- The usage counts of an Episode's stage records, by stage and model.
create function gnomon.read_stage_usage(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    stage text,
    model text,
    calls integer,
    input_tokens bigint,
    output_tokens bigint,
    cost_usd numeric
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select u.stage, u.model, u.calls, u.input_tokens, u.output_tokens, u.cost_usd
    from gnomon.stage_usage u
    where u.tenant_id = p_tenant_id and u.episode_id = p_episode_id
    order by u.stage, u.model
  $$;

-- A refused write raises SQLSTATE GNW01 with a message that says why, as on
-- every other table.

-- A new Summary is of a Node the caller can see, and its Recorded time
-- overlaps no other Summary of that Node in its Scope: none is current, and
-- none was closed after the current transaction started.
create function gnomon.summary_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if not exists (select from gnomon.node n
                   where n.tenant_id = new.tenant_id and n.id = new.node_id) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Node %s is not visible to the caller', new.node_id);
    end if;
    if exists (select from gnomon.summary s
               where s.tenant_id = new.tenant_id and s.scope = new.scope
                 and s.node_id = new.node_id and s.id <> new.id
                 and s.recorded_to > new.recorded_from) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'Node %s has a Summary in Scope %s whose Recorded time a new one would overlap; '
          'close it first', new.node_id, new.scope);
    end if;
    return new;
  end
  $$;

create trigger summary_check_insert before insert on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.summary_check_insert();

-- A Citation or a member is added only to a Summary or Community the current
-- transaction wrote, so what a row cites and holds is fixed when it commits.
-- `tg_argv[0]` names the parent table and `tg_argv[1]` the column naming it.
create function gnomon.derived_check_insert() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_parent uuid := (pg_catalog.to_jsonb(new) ->> tg_argv[1])::uuid;
    v_written_in xid8;
  begin
    if tg_argv[0] = 'summary' then
      select s.written_in into v_written_in from gnomon.summary s
        where s.tenant_id = new.tenant_id and s.scope = new.scope and s.id = v_parent;
    else
      select c.written_in into v_written_in from gnomon.community c
        where c.tenant_id = new.tenant_id and c.scope = new.scope and c.id = v_parent;
    end if;
    if v_written_in is distinct from pg_current_xact_id() then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'a %s''s Citations and Nodes are written with it, and %s was written earlier',
          case when tg_argv[0] = 'summary' then 'Summary' else 'Community' end, v_parent);
    end if;
    return new;
  end
  $$;

create trigger summary_citation_check_insert before insert on gnomon.summary_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('summary', 'summary_id');

create trigger community_member_check_insert before insert on gnomon.community_member
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('community', 'community_id');

create trigger community_citation_check_insert before insert on gnomon.community_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('community', 'community_id');

-- At commit, a Summary that still stands cites an Episode, and a Community
-- cites an Episode and holds a Node. Erasure deletes a Summary or Community
-- with its Citations, so none is left uncited.
create function gnomon.derived_check_complete() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if tg_table_name = 'summary' then
      if exists (select from gnomon.summary s
                 where s.tenant_id = new.tenant_id and s.id = new.id)
         and not exists (select from gnomon.summary_citation c
                         where c.tenant_id = new.tenant_id and c.scope = new.scope
                           and c.summary_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Summary %s cites no Episode', new.id);
      end if;
    elsif exists (select from gnomon.community c
                  where c.tenant_id = new.tenant_id and c.id = new.id) then
      if not exists (select from gnomon.community_citation c
                     where c.tenant_id = new.tenant_id and c.scope = new.scope
                       and c.community_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Community %s cites no Episode', new.id);
      end if;
      if not exists (select from gnomon.community_member m
                     where m.tenant_id = new.tenant_id and m.scope = new.scope
                       and m.community_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Community %s holds no Node', new.id);
      end if;
    end if;
    return null;
  end
  $$;

create constraint trigger summary_check_complete after insert on gnomon.summary
  deferrable initially deferred
  for each row execute function gnomon.derived_check_complete();

create constraint trigger community_check_complete after insert on gnomon.community
  deferrable initially deferred
  for each row execute function gnomon.derived_check_complete();

-- A Summary or Community changes only by closing: its Recorded-time end goes
-- from open to the start of the current transaction, after its start.
create function gnomon.derived_guard_update() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if (pg_catalog.to_jsonb(new) - 'recorded_to') is distinct from
       (pg_catalog.to_jsonb(old) - 'recorded_to') then
      raise exception using errcode = 'insufficient_privilege',
        message = pg_catalog.format(
          'a %s is immutable apart from the end of its Recorded time', tg_argv[0]);
    end if;
    if old.recorded_to = 'infinity' and new.recorded_to = transaction_timestamp()
       and old.recorded_from < new.recorded_to then
      return new;
    end if;
    raise exception using errcode = 'insufficient_privilege',
      message = pg_catalog.format(
        'a %s''s Recorded time may only be closed, at the start of the current transaction',
        tg_argv[0]);
  end
  $$;

create trigger summary_guard_update before update on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_guard_update('Summary');

create trigger community_guard_update before update on gnomon.community
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_guard_update('Community');

-- Only the transaction that wrote a row deletes it. Committed rows go only
-- by Erasure (ADR-0009).
create trigger summary_guard_delete before delete on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Summary');

create trigger summary_citation_guard_delete before delete on gnomon.summary_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');

create trigger community_guard_delete before delete on gnomon.community
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Community');

create trigger community_member_guard_delete before delete on gnomon.community_member
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Community member');

create trigger community_citation_guard_delete before delete on gnomon.community_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');

-- The Citations a write names, checked: at least one, each of an Episode the
-- caller sees in `p_scope`, with that Episode's Reference time and Recorded
-- time. `p_properties` and `p_places` are as long as `p_episodes`, or null
-- for none. An Episode of another Scope the caller sees is refused by name;
-- one the caller cannot see is not visible, wherever it is.
create function gnomon.derived_citations(
  p_tenant_id uuid,
  p_scope text,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns table (
    episode_id uuid,
    property text,
    place text,
    episode_reference_time timestamptz,
    episode_recorded_at timestamptz
  )
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    n integer := coalesce(pg_catalog.cardinality(p_episodes), 0);
    i integer;
    e record;
  begin
    if n = 0 then
      raise exception using errcode = 'GNW01',
        message = 'a Summary or Community cites at least one Episode';
    end if;
    if pg_catalog.cardinality(p_properties) is distinct from n and p_properties is not null
       or pg_catalog.cardinality(p_places) is distinct from n and p_places is not null then
      raise exception using errcode = 'GNW01',
        message = 'each Citation names its Episode, and its property and place or none';
    end if;
    for i in 1 .. n loop
      if p_episodes[i] is null then
        raise exception using errcode = 'GNW01', message = 'a Citation names its Episode';
      end if;
      select x.scope, x.reference_time, x.recorded_at into e
        from gnomon.episode x
        where x.tenant_id = p_tenant_id and x.id = p_episodes[i];
      if not found then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Episode %s is not visible to the caller', p_episodes[i]);
      end if;
      if e.scope <> p_scope then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Episode %s is in Scope %s, and a Summary or Community in Scope %s cites only '
            'Episodes of its own Scope', p_episodes[i], e.scope, p_scope);
      end if;
      episode_id := p_episodes[i];
      property := p_properties[i];
      place := p_places[i];
      episode_reference_time := e.reference_time;
      episode_recorded_at := e.recorded_at;
      return next;
    end loop;
  end
  $$;

-- The Tenant and Scope of a write, checked before anything is read.
create function gnomon.check_write_scope(p_tenant_id uuid, p_scope text) returns void
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a write may write only in the caller''s own Tenant';
    end if;
    if p_scope is null or not (p_scope = any(gnomon.allowed_scopes())) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Scope %s is not visible to the caller', p_scope);
    end if;
  end
  $$;

-- Writes a Summary of a Node in a Scope of the caller's and returns its ID.
-- The Node's current Summary in that Scope, if any, is closed, or deleted
-- when the current transaction wrote it. A current Summary that a
-- transaction started later than this one wrote fails the write with a
-- serialization failure, which the caller retries.
create function gnomon.write_summary(
  p_tenant_id uuid,
  p_scope text,
  p_node_id uuid,
  p_content text,
  p_as_of_valid timestamptz,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns uuid
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_old record;
    v_id uuid;
  begin
    perform gnomon.check_write_scope(p_tenant_id, p_scope);
    perform from gnomon.lock_for_write(p_tenant_id, p_node_id, false);
    select s.id, s.recorded_from, s.written_in into v_old
      from gnomon.summary s
      where s.tenant_id = p_tenant_id and s.scope = p_scope and s.node_id = p_node_id
        and s.recorded_to = 'infinity';
    if found then
      if v_old.written_in = pg_current_xact_id() then
        delete from gnomon.summary_citation c
          where c.tenant_id = p_tenant_id and c.scope = p_scope and c.summary_id = v_old.id;
        delete from gnomon.summary s where s.tenant_id = p_tenant_id and s.id = v_old.id;
      elsif v_old.recorded_from >= t then
        raise exception using errcode = 'serialization_failure',
          message = pg_catalog.format(
            'a later transaction wrote the Summary of Node %s in Scope %s first; retry the write',
            p_node_id, p_scope);
      else
        update gnomon.summary s set recorded_to = t
          where s.tenant_id = p_tenant_id and s.id = v_old.id;
      end if;
    end if;
    insert into gnomon.summary (tenant_id, scope, node_id, content, as_of_valid)
      values (p_tenant_id, p_scope, p_node_id, p_content, p_as_of_valid)
      returning id into v_id;
    insert into gnomon.summary_citation
        (tenant_id, scope, summary_id, episode_id, property, place,
         episode_reference_time, episode_recorded_at)
      select distinct p_tenant_id, p_scope, v_id, c.episode_id, c.property, c.place,
             c.episode_reference_time, c.episode_recorded_at
      from gnomon.derived_citations(p_tenant_id, p_scope, p_episodes, p_properties, p_places) c;
    return v_id;
  end
  $$;

-- Writes a Community of Nodes in a Scope of the caller's, with its Summary,
-- and returns its ID. Every Node is in that Scope (ADR-0014). The current
-- Communities `p_replaces` names, all of that Scope, are closed, or deleted
-- when the current transaction wrote them.
create function gnomon.write_community(
  p_tenant_id uuid,
  p_scope text,
  p_replaces uuid[],
  p_members uuid[],
  p_content text,
  p_as_of_valid timestamptz,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns uuid
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_members uuid[] := array(select distinct m from pg_catalog.unnest(p_members) m order by m);
    v_member uuid;
    v_member_scope text;
    v_old record;
    v_id uuid;
  begin
    perform gnomon.check_write_scope(p_tenant_id, p_scope);
    if pg_catalog.cardinality(v_members) = 0
       or pg_catalog.array_position(v_members, null) is not null then
      raise exception using errcode = 'GNW01',
        message = 'a Community holds at least one Node, and names each';
    end if;
    foreach v_member in array v_members loop
      select l.scope into v_member_scope
        from gnomon.lock_for_write(p_tenant_id, v_member, false) l;
      if v_member_scope <> p_scope then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Node %s is in Scope %s, and a Community in Scope %s holds only Nodes of its own '
            'Scope', v_member, v_member_scope, p_scope);
      end if;
    end loop;
    for v_old in
      select r.id, c.recorded_from, c.recorded_to, c.written_in
      from (select distinct x as id from pg_catalog.unnest(p_replaces) x where x is not null) r
      left join gnomon.community c
        on c.tenant_id = p_tenant_id and c.scope = p_scope and c.id = r.id
      order by r.id
    loop
      if v_old.recorded_to is distinct from 'infinity' then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Community %s is not a current Community of Scope %s the caller sees',
            v_old.id, p_scope);
      end if;
      if v_old.written_in = pg_current_xact_id() then
        delete from gnomon.community_citation c
          where c.tenant_id = p_tenant_id and c.scope = p_scope and c.community_id = v_old.id;
        delete from gnomon.community_member m
          where m.tenant_id = p_tenant_id and m.scope = p_scope and m.community_id = v_old.id;
        delete from gnomon.community c where c.tenant_id = p_tenant_id and c.id = v_old.id;
      elsif v_old.recorded_from >= t then
        raise exception using errcode = 'serialization_failure',
          message = pg_catalog.format(
            'a later transaction wrote Community %s first; retry the write', v_old.id);
      else
        update gnomon.community c set recorded_to = t
          where c.tenant_id = p_tenant_id and c.id = v_old.id and c.recorded_to = 'infinity';
        if not found then
          raise exception using errcode = 'serialization_failure',
            message = pg_catalog.format(
              'another transaction replaced Community %s first; retry the write', v_old.id);
        end if;
      end if;
    end loop;
    insert into gnomon.community (tenant_id, scope, content, as_of_valid)
      values (p_tenant_id, p_scope, p_content, p_as_of_valid)
      returning id into v_id;
    insert into gnomon.community_member (tenant_id, scope, community_id, node_id)
      select p_tenant_id, p_scope, v_id, m from pg_catalog.unnest(v_members) m;
    insert into gnomon.community_citation
        (tenant_id, scope, community_id, episode_id, property, place,
         episode_reference_time, episode_recorded_at)
      select distinct p_tenant_id, p_scope, v_id, c.episode_id, c.property, c.place,
             c.episode_reference_time, c.episode_recorded_at
      from gnomon.derived_citations(p_tenant_id, p_scope, p_episodes, p_properties, p_places) c;
    return v_id;
  end
  $$;

-- The Summaries of a Node As-of `p_valid_at` and `p_known_at`, one in each
-- of the caller's Scopes that has one, with their Citations. A null
-- `p_known_at` reads the current Summaries. A Summary is found only at a
-- valid-at instant on or after the one it was written for (ADR-0015).
create function gnomon.read_summaries(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    scope text,
    node_id uuid,
    content text,
    as_of_valid timestamptz,
    recorded_from timestamptz,
    recorded_to timestamptz,
    cited_episodes uuid[],
    cited_properties text[],
    cited_places text[]
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select s.id, s.scope, s.node_id, s.content, s.as_of_valid, s.recorded_from,
           nullif(s.recorded_to, 'infinity'),
           coalesce(c.episodes, '{}'), coalesce(c.properties, '{}'), coalesce(c.places, '{}')
    from gnomon.summary s
    cross join lateral (
      select pg_catalog.array_agg(x.episode_id order by x.episode_id, x.property, x.place) as episodes,
             pg_catalog.array_agg(x.property order by x.episode_id, x.property, x.place) as properties,
             pg_catalog.array_agg(x.place order by x.episode_id, x.property, x.place) as places
      from gnomon.summary_citation x
      where x.tenant_id = s.tenant_id and x.scope = s.scope and x.summary_id = s.id
    ) c
    where s.tenant_id = p_tenant_id and s.node_id = p_node_id
      and s.as_of_valid <= p_valid_at
      and case
            when p_known_at is null then s.recorded_to = 'infinity'
            else s.recorded_from <= p_known_at and s.recorded_to > p_known_at
          end
    order by s.scope
  $$;

-- The Communities that hold a Node As-of `p_valid_at` and `p_known_at`, with
-- their Nodes and Citations, as `gnomon.read_summaries` reads Summaries.
create function gnomon.read_communities(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    scope text,
    members uuid[],
    content text,
    as_of_valid timestamptz,
    recorded_from timestamptz,
    recorded_to timestamptz,
    cited_episodes uuid[],
    cited_properties text[],
    cited_places text[]
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select k.id, k.scope,
           array(select m.node_id from gnomon.community_member m
                 where m.tenant_id = k.tenant_id and m.scope = k.scope and m.community_id = k.id
                 order by m.node_id),
           k.content, k.as_of_valid, k.recorded_from, nullif(k.recorded_to, 'infinity'),
           coalesce(c.episodes, '{}'), coalesce(c.properties, '{}'), coalesce(c.places, '{}')
    from gnomon.community_member h
    join gnomon.community k
      on k.tenant_id = h.tenant_id and k.scope = h.scope and k.id = h.community_id
    cross join lateral (
      select pg_catalog.array_agg(x.episode_id order by x.episode_id, x.property, x.place) as episodes,
             pg_catalog.array_agg(x.property order by x.episode_id, x.property, x.place) as properties,
             pg_catalog.array_agg(x.place order by x.episode_id, x.property, x.place) as places
      from gnomon.community_citation x
      where x.tenant_id = k.tenant_id and x.scope = k.scope and x.community_id = k.id
    ) c
    where h.tenant_id = p_tenant_id and h.node_id = p_node_id
      and k.as_of_valid <= p_valid_at
      and case
            when p_known_at is null then k.recorded_to = 'infinity'
            else k.recorded_from <= p_known_at and k.recorded_to > p_known_at
          end
    order by k.scope, k.id
  $$;

insert into gnomon.schema_version (version, name) values (3, '0003_reads_and_writes');
