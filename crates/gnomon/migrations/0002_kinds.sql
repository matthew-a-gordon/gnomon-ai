-- gnomon 0002: Kinds. The root and reserved Kinds, Kind registration and
-- its JSON Schema profile, Kind reads and lineage, and the merge rules
-- (ADR-0002, ADR-0007, ADR-0012).
--
-- A Kind is a named class of Node or of Edge. It declares its properties in
-- a profile of JSON Schema 2020-12, may specialize one parent Kind, and, for
-- an Edge Kind, names the Node Kinds it may join. Registering a Kind rejects
-- any keyword outside the profile, flattens the Kind with its ancestors, and
-- compiles one jsonpath rule per property. The rules are stored as data with
-- the Kind, and the Store's write functions evaluate them with
-- `jsonb_path_match`, so every host accepts or refuses the same write
-- (ADR-0002).
--
-- An Ontology belongs to one Tenant and holds every Kind that Tenant
-- registers. Kinds carry a Tenant and no Scope: a Scope partitions a Tenant's
-- graph, and every Scope of a Tenant shares one Ontology (ADR-0005).
--
-- Every Ontology has a root Kind for Nodes, `node`, and one for Edges,
-- `edge`. They are not rows: `gnomon.root_kinds()` defines them once for
-- every Tenant, so any Node or Edge can be written before a Tenant registers
-- a Kind of its own.
--
-- Kind registration is one of the four SECURITY DEFINER groups (ADR-0009).
-- It is owned by `gnomon_definer`, a NOLOGIN role that cannot bypass RLS,
-- and checks the Tenant against `gnomon.tenant_id()`. The files grant no
-- caller role the right to execute it: each deployment grants that to the
-- roles it chooses. The role that applies the files needs CREATEROLE, or
-- must already be able to SET ROLE to an existing `gnomon_definer`, so that
-- it can create the role and hand it the function.
--
-- Ranked merge and Authority. A Kind can turn ranked merge on. A Node of a
-- Kind that ranks has its Versions' values chosen by Authority, then
-- Reference time, then Recorded time (ADR-0007). Without ranked merge the
-- order is Reference time, then Recorded time. Authority is an integer the
-- consumer gives an Episode, and an Episode with none ranks as 0.
--
-- Authority decides only between Contributions whose Valid times overlap.
-- The merge cuts a Node's Valid time into segments at every Contribution
-- bound and picks each property's winner in each segment, so a period that
-- only one Contribution covers keeps that Contribution whatever its
-- Authority, and Authority never reaches across a gap.
--
-- Authority ranks statements as it ranks values. A null from a
-- lower-Authority Episode, an End of validity, loses to the values a
-- higher-Authority Episode supplied where their Valid times overlap, and a
-- bound statement moves a Contribution's bound only if its Episode's
-- Authority is at least that of the Contribution's Episode. A losing
-- statement is never deleted: it stays held, and takes effect if the
-- higher-Authority Episode is erased.
--
-- The rule is a property of a Kind. A child Kind inherits its ancestors'
-- rule: it ranks if it or any ancestor does, so a child Kind may turn ranked
-- merge on and may not turn it off while an ancestor ranks. Each change of a
-- Kind's own rule is a new row of `gnomon.kind_merge`, the latest of which
-- holds, so no row is updated. Only `gnomon.set_merge_rule` writes it, a
-- SECURITY DEFINER function like Kind registration (ADR-0009), granted to no
-- caller role.
--
-- Each Version records the rule that chose its values in `merge_rule`, and a
-- change to a Kind's rule applies to later writes only: it changes no
-- existing Version. A write recomputes its Valid time widened to the whole of
-- every open Version it overlaps or touches, so a Version outside that window
-- keeps the rule it was written under, and one inside it is chosen again
-- under the Kind's rule at the time of the write. A Version that choice
-- leaves unchanged keeps its recorded rule. Erasure recomputes each Version
-- under its recorded rule.
--
-- Bound statements are read as stored. A statement from an Episode that
-- loses on Authority is recorded as a Refinement or a Correction as the
-- earlier bound decides, even though the merge ignores it, so one
-- `state_bound` call can return `correction` while some of the rows it wrote
-- are held.
--
-- The reserved Edge Kinds `same_as` and `different_from`. Resolution records
-- its findings as Edges, not as merged Nodes (ADR-0012). When two Nodes
-- prove to be one thing, a `same_as` Edge says so, and a `different_from`
-- Edge records that two are not. Every Ontology has both Kinds, each joining
-- any Node to any other Node. They are ordinary Edges, so Valid time,
-- Citations, Retraction and Erasure apply to them with no new mechanism, and
-- reads do not collapse the Nodes they join.
--
-- Like the root Kinds, the reserved Kinds are not rows:
-- `gnomon.reserved_kinds()` defines them once for every Tenant, so a Tenant
-- has them from the moment it first appears, whenever that is, and no
-- consumer registers them. Each is an Edge Kind below the root Kind `edge`,
-- with no properties of its own.
--
-- Both are symmetric: an Edge of either joins its two Nodes in no direction.
-- The Store keeps one such Edge per Kind, pair of Nodes and Scope, with its
-- endpoints stored in a canonical order, the lower Node ID as the source.
-- Creating one from A to B and then from B to A returns the same Edge, and a
-- traversal follows it from either Node in either direction. A Retraction
-- ends it and it stays the same Edge, so a later write states that it holds
-- again. An Erasure deletes it, and creating it again creates a new Edge.
-- The Edge lives in one Scope like any Edge, so the same pair of Nodes may
-- have one in each Scope.
--
-- No consumer can remove, redefine or reuse the two names. They are not
-- rows, so there is nothing to delete or rewrite, and Kind registration
-- refuses to register a Kind of either name: a trigger on `gnomon.kind`
-- refuses the row it would insert. A Kind cannot specialize one, because
-- registration finds no registered parent of that name, and no Edge Kind
-- can name one as an endpoint, because neither is a Node Kind. The
-- merge-rule setter finds no registered Kind of either name either, so both
-- keep the recency rule.

-- The root Kinds every Ontology has. Node Kinds search the name and Aliases
-- by default and Edge Kinds the Sentence (ADR-0002, ADR-0010); child Kinds
-- inherit these marks. The root Edge Kind joins any Node to any Node.
create function gnomon.root_kinds()
  returns table (
    name text,
    kind_of text,
    search text[],
    source_kinds text[],
    target_kinds text[]
  )
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    values
      ('node', 'node', array['name', 'aliases'], null::text[], null::text[]),
      ('edge', 'edge', array['sentence'], array['node'], array['node'])
  $$;

-- The keywords of the JSON Schema profile a Kind's properties may use
-- (ADR-0002). `format` is limited to date, date-time and uuid.
create function gnomon.kind_profile_keywords()
  returns text[]
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select array[
      'type', 'properties', 'required', 'enum', 'const', 'items',
      'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum',
      'minLength', 'maxLength', 'minItems', 'maxItems', 'anyOf', 'format'
    ]
  $$;

-- A property name as one JSON Pointer token (RFC 6901), for error messages.
create function gnomon.json_pointer_token(p_key text)
  returns text
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$ select replace(replace(p_key, '~', '~0'), '/', '~1') $$;

-- Validates one schema of the profile and compiles it to a jsonpath
-- predicate over `p_subject`, a jsonpath expression such as `$`, `$."a"` or
-- `@`. `p_at` is the schema's JSON Pointer, for error messages.
--
-- The profile is JSON Schema 2020-12 limited to: type, properties, required,
-- enum, const, items, minimum, maximum, exclusiveMinimum, exclusiveMaximum,
-- minLength, maxLength, minItems, maxItems, anyOf, and format limited to
-- date, date-time and uuid (ADR-0002). Any other keyword is refused.
--
-- The predicate is evaluated in strict mode, because lax mode unwraps an
-- array inside an array and would check `[[1]]` as if it were `[1]`. Each
-- keyword applies only to values of the JSON type it constrains, as in JSON
-- Schema, so every check is guarded by a type test, and a property's
-- presence is tested through `keyvalue()`, which does not fail on a missing
-- key. Where jsonpath yields unknown, as when comparing a string with a
-- number, the value is treated as invalid: `items` refuses an element unless
-- its predicate is true, and the property rule built from this predicate
-- maps unknown to false.
create function gnomon.compile_kind_schema(p_schema jsonb, p_subject text, p_at text)
  returns text
  language plpgsql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    supported constant text[] := gnomon.kind_profile_keywords();
    type_names constant text[] := array[
      'string', 'number', 'integer', 'boolean', 'null', 'array', 'object'
    ];
    -- Postgres regular expressions count at most 255 repetitions, and
    -- longer counts cost about a millisecond per write at 1,000 characters,
    -- growing fast beyond it, so length bounds stop here.
    max_length_bound constant integer := 1000;
    -- `items` states its element predicate twice, so each level of arrays
    -- nested in arrays doubles the rule. These limits stop a schema from
    -- making registration build an arbitrarily large rule. The length is
    -- checked at every level, so no rule built here exceeds twice it.
    max_depth constant integer := 32;
    max_rule_length constant integer := 65536;
    s constant text := p_subject;
    result text;
    checks text[] := '{}';
    k text;
    v jsonb;
    e jsonb;
    parts text[];
    n numeric;
    i integer;
  begin
    if jsonb_typeof(p_schema) is distinct from 'object' then
      raise exception using errcode = 'GNK01',
        message = format('the schema at %s must be a JSON object', p_at);
    end if;
    -- Every level of nesting adds at least one segment to the pointer, and
    -- an escaped key holds no '/'.
    if length(p_at) - length(replace(p_at, '/', '')) > 2 * max_depth then
      raise exception using errcode = 'GNK01',
        message = format(
          'the schema at %s nests deeper than the profile allows (%s levels)', p_at, max_depth);
    end if;

    for k in select jsonb_object_keys(p_schema) order by 1 loop
      if not k = any(supported) then
        raise exception using errcode = 'GNK01',
          message = format(
            'unsupported JSON Schema keyword "%s" at %s; a Kind''s properties may use only %s, '
            'with format limited to date, date-time and uuid',
            k, p_at, array_to_string(supported, ', '));
      end if;
    end loop;

    -- type: a type name or a non-empty list of them.
    if p_schema ? 'type' then
      v := p_schema -> 'type';
      if jsonb_typeof(v) = 'string' then
        parts := array[v #>> '{}'];
      elsif jsonb_typeof(v) = 'array' and jsonb_array_length(v) > 0
        and not exists (select from jsonb_array_elements(v) x where jsonb_typeof(x) <> 'string')
        and (select count(distinct x) = count(*) from jsonb_array_elements(v) x)
      then
        parts := array(select jsonb_array_elements_text(v));
      else
        raise exception using errcode = 'GNK01',
          message = format('"type" at %s must be a type name or a non-empty list of distinct ones', p_at);
      end if;
      foreach k in array parts loop
        if not k = any(type_names) then
          raise exception using errcode = 'GNK01',
            message = format(
              'unknown type "%s" at %s; the types are string, number, integer, boolean, null, array and object',
              k, p_at);
        end if;
      end loop;
      checks := checks || ('(' || array_to_string(array(
        select case
          when t = 'integer' then format('(%1$s.type() == "number" && %1$s.floor() == %1$s)', s)
          else format('%s.type() == %s', s, to_jsonb(t)::text)
        end
        from unnest(parts) t
      ), ' || ') || ')');
    end if;

    -- properties: each present property satisfies its own schema.
    if p_schema ? 'properties' then
      v := p_schema -> 'properties';
      if jsonb_typeof(v) <> 'object' then
        raise exception using errcode = 'GNK01',
          message = format('"properties" at %s must be an object of schemas', p_at);
      end if;
      for k, e in select key, value from jsonb_each(v) order by key loop
        checks := checks || format(
          '(!(%1$s.type() == "object") || !exists(%1$s.keyvalue() ? (@.key == %2$s)) || (%3$s))',
          s,
          to_jsonb(k)::text,
          gnomon.compile_kind_schema(
            e,
            s || '.' || to_jsonb(k)::text,
            p_at || '/properties/' || gnomon.json_pointer_token(k)
          )
        );
      end loop;
    end if;

    -- required: a list of distinct property names.
    if p_schema ? 'required' then
      v := p_schema -> 'required';
      if jsonb_typeof(v) <> 'array'
        or exists (select from jsonb_array_elements(v) x where jsonb_typeof(x) <> 'string')
        or (select count(distinct x) <> count(*) from jsonb_array_elements_text(v) x)
      then
        raise exception using errcode = 'GNK01',
          message = format('"required" at %s must be a list of distinct property names', p_at);
      end if;
      if jsonb_array_length(v) > 0 then
        checks := checks || format(
          '(!(%s.type() == "object") || (%s))',
          s,
          array_to_string(array(
            select format('exists(%s.keyvalue() ? (@.key == %s))', s, to_jsonb(x)::text)
            from jsonb_array_elements_text(v) x
          ), ' && ')
        );
      end if;
    end if;

    -- enum and const: jsonpath compares scalars only, so the values must be
    -- strings, numbers, booleans or null.
    foreach k in array array['enum', 'const'] loop
      if p_schema ? k then
        v := case when k = 'const' then jsonb_build_array(p_schema -> k) else p_schema -> k end;
        if jsonb_typeof(v) <> 'array' or jsonb_array_length(v) = 0 then
          raise exception using errcode = 'GNK01',
            message = format('"enum" at %s must be a non-empty list', p_at);
        end if;
        if exists (select from jsonb_array_elements(v) x where jsonb_typeof(x) in ('object', 'array')) then
          raise exception using errcode = 'GNK01',
            message = format(
              '"%s" at %s may hold only strings, numbers, booleans or null', k, p_at);
        end if;
        checks := checks || format(
          '(%1$s.type() != "object" && %1$s.type() != "array" && (%2$s))',
          s,
          array_to_string(array(
            select format('%s == %s', s, x::text) from jsonb_array_elements(v) x
          ), ' || ')
        );
      end if;
    end loop;

    -- items: one schema that every element satisfies.
    if p_schema ? 'items' then
      v := p_schema -> 'items';
      if jsonb_typeof(v) <> 'object' then
        raise exception using errcode = 'GNK01',
          message = format(
            '"items" at %s must be one schema; tuple items are not in the profile', p_at);
      end if;
      k := gnomon.compile_kind_schema(v, '@', p_at || '/items');
      checks := checks || format(
        '(!(%1$s.type() == "array") || !exists(%1$s[*] ? (!(%2$s) || (%2$s) is unknown)))',
        s, k
      );
    end if;

    -- Numeric bounds.
    foreach k in array array['minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum'] loop
      if p_schema ? k then
        v := p_schema -> k;
        if jsonb_typeof(v) <> 'number' then
          raise exception using errcode = 'GNK01',
            message = format('"%s" at %s must be a number', k, p_at);
        end if;
        checks := checks || format(
          '(!(%1$s.type() == "number") || %1$s %2$s %3$s)',
          s,
          case k
            when 'minimum' then '>='
            when 'maximum' then '<='
            when 'exclusiveMinimum' then '>'
            else '<'
          end,
          v::text
        );
      end if;
    end loop;

    -- Length bounds, of strings and of arrays.
    foreach k in array array['minLength', 'maxLength', 'minItems', 'maxItems'] loop
      if p_schema ? k then
        v := p_schema -> k;
        if jsonb_typeof(v) <> 'number' or (v::text)::numeric < 0
          or (v::text)::numeric <> trunc((v::text)::numeric)
        then
          raise exception using errcode = 'GNK01',
            message = format('"%s" at %s must be a whole number of at least 0', k, p_at);
        end if;
        n := (v::text)::numeric;
        if k in ('minLength', 'maxLength') then
          if n > max_length_bound then
            raise exception using errcode = 'GNK01',
              message = format(
                '"%s" at %s is %s; the Store enforces string length bounds up to %s',
                k, p_at, n, max_length_bound);
          end if;
          -- A string has at least i characters when it starts with i of
          -- them. Counts above 255 are split into groups of 255.
          i := case when k = 'minLength' then n::integer else n::integer + 1 end;
          if i > 0 then
            e := to_jsonb(
              '^'
              || case when i > 255 then format('(.{255}){%s}', i / 255) else '' end
              || format('.{%s}', case when i > 255 then i % 255 else i end)
            );
            checks := checks || format(
              '(!(%1$s.type() == "string") || %2$s(%1$s like_regex %3$s flag "s"))',
              s,
              case when k = 'minLength' then '' else '!' end,
              e::text
            );
          end if;
        else
          checks := checks || format(
            '(!(%1$s.type() == "array") || %1$s.size() %2$s %3$s)',
            s, case when k = 'minItems' then '>=' else '<=' end, n
          );
        end if;
      end if;
    end loop;

    -- anyOf: at least one of a non-empty list of schemas.
    if p_schema ? 'anyOf' then
      v := p_schema -> 'anyOf';
      if jsonb_typeof(v) <> 'array' or jsonb_array_length(v) = 0 then
        raise exception using errcode = 'GNK01',
          message = format('"anyOf" at %s must be a non-empty list of schemas', p_at);
      end if;
      checks := checks || ('(' || array_to_string(array(
        select '(' || gnomon.compile_kind_schema(x, s, p_at || '/anyOf/' || (o - 1)) || ')'
        from jsonb_array_elements(v) with ordinality as a(x, o)
        order by o
      ), ' || ') || ')');
    end if;

    -- format: date (RFC 3339 full-date), date-time (RFC 3339 date-time with
    -- an upper-case T and Z and at most microseconds, the precision Postgres
    -- keeps), and uuid. The pattern fixes the shape and the datetime parse
    -- rejects dates that do not exist.
    if p_schema ? 'format' then
      v := p_schema -> 'format';
      if jsonb_typeof(v) <> 'string' or (v #>> '{}') not in ('date', 'date-time', 'uuid') then
        raise exception using errcode = 'GNK01',
          message = format(
            'unsupported format %s at %s; the profile allows date, date-time and uuid', v, p_at);
      end if;
      checks := checks || format(
        '(!(%1$s.type() == "string") || (%2$s))',
        s,
        case v #>> '{}'
          when 'uuid' then format(
            '%s like_regex %s', s,
            to_jsonb('^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'::text)::text)
          when 'date' then format(
            '%1$s like_regex %2$s && %1$s.datetime("YYYY-MM-DD").type() == "date"', s,
            to_jsonb('^[0-9]{4}-[0-9]{2}-[0-9]{2}$'::text)::text)
          else format(
            '%1$s like_regex %2$s && %1$s.datetime().type() == "timestamp with time zone"', s,
            to_jsonb('^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?(Z|[+-][0-9]{2}:[0-9]{2})$'::text)::text)
        end
      );
    end if;

    if cardinality(checks) = 0 then
      return '(1 == 1)';
    end if;
    result := array_to_string(checks, ' && ');
    if length(result) > max_rule_length then
      raise exception using errcode = 'GNK01',
        message = format(
          'the schema at %s compiles to a rule longer than %s characters; nest fewer arrays in arrays',
          coalesce(nullif(p_at, ''), 'the top level'), max_rule_length);
    end if;
    return result;
  end
  $$;

-- The rule stored for one property: the compiled predicate over `$`, with
-- unknown mapped to false, so `jsonb_path_match` is always true or false.
create function gnomon.compile_kind_property(p_schema jsonb, p_at text)
  returns jsonpath
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select format(
      'strict !(!(%1$s) || (%1$s) is unknown)',
      gnomon.compile_kind_schema(p_schema, '$', p_at)
    )::jsonpath
  $$;

-- Registers a Kind in the caller's Tenant.
--
--   p_kind_of       'node' or 'edge'
--   p_parent        the parent Kind, or null for the root Kind of p_kind_of
--   p_properties    {"type": "object", "properties": {...}, "required": [...]}
--                   in the JSON Schema profile; every key is optional
--   p_search        the properties this Kind adds to Search; it also inherits
--                   its parent's
--   p_source_kinds  for an Edge Kind, the Node Kinds it may start at, each
--   p_target_kinds  equal to or below one its parent allows; null or empty
--                   inherits the parent's. Null for a Node Kind.
--
-- Registering a Kind again with the same arguments changes nothing. A Kind
-- registered once cannot be registered again with other arguments.
--
-- This is a definer function, owned by gnomon_definer, so that callers need
-- no write grant on the Kind tables. It refuses any Tenant but the caller's,
-- and RLS checks the Tenant again on every row it writes.
create function gnomon.register_kind(
  p_tenant_id uuid,
  p_name text,
  p_kind_of text,
  p_parent text,
  p_properties jsonb,
  p_search text[],
  p_source_kinds text[],
  p_target_kinds text[]
) returns void
  language plpgsql volatile security definer set search_path = pg_catalog, pg_temp
  as $$
  declare
    reserved constant text[] := array['name', 'aliases', 'sentence'];
    parent_name text;
    parent_ancestors text[];
    parent_properties jsonb;
    parent_required jsonb;
    parent_search text[];
    parent_source text[];
    parent_target text[];
    own jsonb;
    props jsonb;
    req jsonb;
    marks text[];
    source_kinds text[];
    target_kinds text[];
    def jsonb;
    existing jsonb;
    rules jsonb;
    k text;
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'Kind registration may register a Kind only in the caller''s own Tenant';
    end if;

    if p_name is null or p_name !~ '^[A-Za-z][A-Za-z0-9_]{0,62}$' then
      raise exception using errcode = 'GNK01',
        message = format(
          'Kind name %s must be a letter followed by at most 62 letters, digits or underscores',
          coalesce(quote_literal(p_name), 'null'));
    end if;
    if exists (select from gnomon.root_kinds() r where r.name = p_name) then
      raise exception using errcode = 'GNK01',
        message = format('"%s" is a root Kind, which every Ontology already has', p_name);
    end if;
    if p_kind_of is null or p_kind_of not in ('node', 'edge') then
      raise exception using errcode = 'GNK01',
        message = format('a Kind is of node or of edge, not %s', coalesce(quote_literal(p_kind_of), 'null'));
    end if;

    -- The parent: a root Kind, or a registered Kind.
    parent_name := coalesce(p_parent, p_kind_of);
    if exists (select from gnomon.root_kinds() r where r.name = parent_name) then
      select r.kind_of, r.search, r.source_kinds, r.target_kinds
        into k, parent_search, parent_source, parent_target
        from gnomon.root_kinds() r where r.name = parent_name;
      parent_ancestors := '{}';
      parent_properties := '{}';
      parent_required := '[]';
    else
      select pk.kind_of, pk.search, pk.source_kinds, pk.target_kinds, pk.ancestors,
             pk.properties -> 'properties', pk.properties -> 'required'
        into k, parent_search, parent_source, parent_target, parent_ancestors,
             parent_properties, parent_required
        from gnomon.kind pk
        where pk.tenant_id = p_tenant_id and pk.name = parent_name;
      if not found then
        raise exception using errcode = 'GNK01',
          message = format('parent Kind "%s" is not registered', parent_name);
      end if;
    end if;
    if k <> p_kind_of then
      raise exception using errcode = 'GNK01',
        message = format(
          'a Kind of %s cannot specialize "%s", which is a Kind of %s',
          p_kind_of, parent_name, k);
    end if;

    -- The top level is the object of the Kind's properties.
    if jsonb_typeof(p_properties) is distinct from 'object' then
      raise exception using errcode = 'GNK01',
        message = 'a Kind''s properties must be a JSON Schema object';
    end if;
    for k in select jsonb_object_keys(p_properties) order by 1 loop
      if k not in ('type', 'properties', 'required') then
        if k = any(gnomon.kind_profile_keywords()) then
          raise exception using errcode = 'GNK01',
            message = format(
              '"%s" cannot apply to a whole Kind; the top level of a Kind''s properties '
              'takes only type, properties and required', k);
        end if;
        raise exception using errcode = 'GNK01',
          message = format(
            'unsupported JSON Schema keyword "%s" at the top level; a Kind''s properties '
            'take only type, properties and required there', k);
      end if;
    end loop;
    if p_properties ? 'type' and p_properties -> 'type' <> '"object"' then
      raise exception using errcode = 'GNK01',
        message = 'the top level of a Kind''s properties must have type "object"';
    end if;
    own := coalesce(p_properties -> 'properties', '{}');
    if jsonb_typeof(own) <> 'object' then
      raise exception using errcode = 'GNK01',
        message = '"properties" at the top level must be an object of schemas';
    end if;

    -- Flatten: the parent's properties, then the Kind's own.
    for k in select jsonb_object_keys(own) order by 1 loop
      if parent_properties ? k then
        raise exception using errcode = 'GNK01',
          message = format(
            'property "%s" is inherited from "%s"; a child Kind cannot declare it again',
            k, parent_name);
      end if;
      if k = any(reserved) then
        raise exception using errcode = 'GNK01',
          message = format(
            'property name "%s" is reserved: name and aliases belong to every Node and '
            'sentence to every Edge', k);
      end if;
    end loop;
    props := parent_properties || own;

    req := coalesce(p_properties -> 'required', '[]');
    if jsonb_typeof(req) <> 'array'
      or exists (select from jsonb_array_elements(req) x where jsonb_typeof(x) <> 'string')
      or (select count(distinct x) <> count(*) from jsonb_array_elements(req) x)
    then
      raise exception using errcode = 'GNK01',
        message = '"required" at the top level must be a list of distinct property names';
    end if;
    for k in select jsonb_array_elements_text(req) loop
      if not props ? k then
        raise exception using errcode = 'GNK01',
          message = format('required property "%s" is not a property of the Kind', k);
      end if;
    end loop;
    req := coalesce((
      select jsonb_agg(x order by o)
      from (
        select x, min(o) as o
        from jsonb_array_elements_text(parent_required || req) with ordinality as a(x, o)
        group by x
      ) d
    ), '[]');

    -- Search marks: the parent's, then the Kind's own.
    foreach k in array coalesce(p_search, '{}') loop
      if not (props ? k or k = any(parent_search)) then
        raise exception using errcode = 'GNK01',
          message = format('search mark "%s" names no property of the Kind', k);
      end if;
    end loop;
    marks := array(
      select m from (
        select m, min(o) as o
        from unnest(parent_search || coalesce(p_search, '{}')) with ordinality as a(m, o)
        group by m
      ) d order by o
    );

    -- Endpoints, for an Edge Kind only.
    if p_kind_of = 'node' then
      if p_source_kinds is not null or p_target_kinds is not null then
        raise exception using errcode = 'GNK01',
          message = 'only an Edge Kind names the Node Kinds it may join';
      end if;
    else
      source_kinds := gnomon.kind_endpoints(p_tenant_id, p_source_kinds, parent_source, 'source');
      target_kinds := gnomon.kind_endpoints(p_tenant_id, p_target_kinds, parent_target, 'target');
    end if;

    -- Compile every property before writing anything, so a refused Kind
    -- leaves no row. The inherited properties compiled when the parent
    -- registered, and compile again to the same rules.
    rules := coalesce((
      select jsonb_object_agg(
        e.key,
        gnomon.compile_kind_property(e.value, '/properties/' || gnomon.json_pointer_token(e.key))::text
      )
      from jsonb_each(props) e
    ), '{}');

    def := jsonb_build_object(
      'kind_of', p_kind_of,
      'parent', parent_name,
      'properties', p_properties,
      'search', to_jsonb(coalesce(p_search, '{}')),
      'source_kinds', to_jsonb(coalesce(p_source_kinds, '{}')),
      'target_kinds', to_jsonb(coalesce(p_target_kinds, '{}'))
    );

    insert into gnomon.kind (
      tenant_id, name, kind_of, parent, ancestors, properties, search,
      source_kinds, target_kinds, definition
    ) values (
      p_tenant_id, p_name, p_kind_of, parent_name, parent_ancestors || parent_name,
      jsonb_build_object('type', 'object', 'properties', props, 'required', req),
      marks, source_kinds, target_kinds, def
    )
    on conflict do nothing;

    if not found then
      select x.definition into existing
        from gnomon.kind x where x.tenant_id = p_tenant_id and x.name = p_name;
      if existing = def then
        return;
      end if;
      raise exception using errcode = 'GNK02',
        message = format('Kind "%s" is already registered with a different definition', p_name);
    end if;

    insert into gnomon.kind_rule (tenant_id, kind, property, rule)
      select p_tenant_id, p_name, r.key, r.value::jsonpath
      from jsonb_each_text(rules) r;
  end
  $$;

-- The Node Kinds an Edge Kind may join at one end: the declared ones, each a
-- Node Kind equal to or below one of `p_parent_kinds`, or the parent's when
-- none are declared.
create function gnomon.kind_endpoints(
  p_tenant_id uuid,
  p_declared text[],
  p_parent_kinds text[],
  p_end text
) returns text[]
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    k text;
    lineage text[];
  begin
    if p_declared is null or cardinality(p_declared) = 0 then
      return p_parent_kinds;
    end if;
    foreach k in array p_declared loop
      if exists (select from gnomon.root_kinds() r where r.name = k and r.kind_of = 'node') then
        lineage := array[k];
      else
        select n.ancestors || n.name into lineage
          from gnomon.kind n
          where n.tenant_id = p_tenant_id and n.name = k and n.kind_of = 'node';
        if not found then
          raise exception using errcode = 'GNK01',
            message = format('%s Kind "%s" is not a registered Node Kind', p_end, k);
        end if;
      end if;
      if not lineage && p_parent_kinds then
        raise exception using errcode = 'GNK01',
          message = format(
            '%s Kind "%s" is outside what the parent Edge Kind allows (%s)',
            p_end, k, array_to_string(p_parent_kinds, ', '));
      end if;
    end loop;
    return array(select distinct x from unnest(p_declared) x order by x);
  end
  $$;

-- A refused write raises SQLSTATE GNW01, an invalid write, with a message
-- that says why. The messages come from these files, so every image
-- refuses a write the same way.

-- The properties every Node has whatever its Kind, and their rules: a name,
-- and Aliases, the other names it is known by (ADR-0002).
create function gnomon.node_builtin_rules()
  returns table (property text, rule jsonpath)
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    values
      ('name', 'strict $.type() == "string"'::jsonpath),
      ('aliases', 'strict $.type() == "array" && !exists($[*] ? (@.type() != "string"))'::jsonpath)
  $$;

-- The merge rule a Kind has: `ranked` if it or any ancestor ranks, else
-- `recency`. A root Kind, a Kind the Tenant has not registered and a Kind
-- nobody changed use `recency`.
create function gnomon.kind_merge_rule(p_tenant_id uuid, p_kind text)
  returns text
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case when exists (
      select from gnomon.kind k
      cross join lateral unnest(k.ancestors || k.name) a(name)
      where k.tenant_id = p_tenant_id and k.name = p_kind
        and (select m.merge_rule from gnomon.kind_merge m
              where m.tenant_id = k.tenant_id and m.kind = a.name
              order by m.id desc limit 1) = 'ranked'
    ) then 'ranked' else 'recency' end
  $$;

-- Sets a Kind's own merge rule, `recency` or `ranked`. It applies to later
-- writes only. A Kind below a Kind that ranks cannot turn ranked merge off,
-- and a root Kind has no rule of its own to set.
--
-- This is a definer function, owned by gnomon_definer, so that callers need
-- no write grant on `kind_merge`. It refuses any Tenant but the caller's, and
-- RLS checks the Tenant again on the row it writes. Setting the rule a Kind
-- already has changes nothing.
create function gnomon.set_merge_rule(p_tenant_id uuid, p_kind text, p_rule text)
  returns void
  language plpgsql volatile security definer set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_ancestors text[];
    v_blocker text;
    v_own text;
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'setting a merge rule may set it only in the caller''s own Tenant';
    end if;
    -- Rule changes of a Tenant serialise, so a parent turning ranked on and a
    -- child turning it off cannot both pass their checks.
    perform pg_catalog.pg_advisory_xact_lock(
      pg_catalog.hashtextextended('gnomon.set_merge_rule/' || p_tenant_id::text, 0));
    if p_rule is null or p_rule not in ('recency', 'ranked') then
      raise exception using errcode = 'GNK01',
        message = 'a merge rule is recency or ranked';
    end if;
    if exists (select from gnomon.root_kinds() r where r.name = p_kind) then
      raise exception using errcode = 'GNK01',
        message = format('"%s" is a root Kind, which has no merge rule of its own; register a Kind', p_kind);
    end if;
    select k.ancestors into v_ancestors
      from gnomon.kind k where k.tenant_id = p_tenant_id and k.name = p_kind;
    if not found then
      raise exception using errcode = 'GNK01',
        message = format('Kind "%s" is not registered', p_kind);
    end if;
    if p_rule = 'recency' then
      select a.name into v_blocker
        from unnest(v_ancestors) a(name)
        where (select m.merge_rule from gnomon.kind_merge m
                where m.tenant_id = p_tenant_id and m.kind = a.name
                order by m.id desc limit 1) = 'ranked'
        limit 1;
      if v_blocker is not null then
        raise exception using errcode = 'GNK01',
          message = format(
            'Kind "%s" cannot turn ranked merge off: its ancestor "%s" ranks, and a child Kind '
            'may turn ranked merge on but not off', p_kind, v_blocker);
      end if;
    end if;
    select m.merge_rule into v_own
      from gnomon.kind_merge m
      where m.tenant_id = p_tenant_id and m.kind = p_kind
      order by m.id desc limit 1;
    if coalesce(v_own, 'recency') = p_rule then
      return;
    end if;
    insert into gnomon.kind_merge (tenant_id, kind, merge_rule) values (p_tenant_id, p_kind, p_rule);
  end
  $$;

create function gnomon.kind_check_reserved() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if new.kind_of = 'edge' and new.properties -> 'properties' ? 'holds' then
      raise exception using errcode = 'GNK01',
        message = 'property name "holds" is reserved: every Edge records through it where the Edge holds';
    end if;
    return new;
  end
  $$;

create trigger kind_check_reserved before insert on gnomon.kind
  for each row execute function gnomon.kind_check_reserved();

-- The property every Edge has whatever its Kind, and its rule: `holds`,
-- which only true states. A JSON null ends it, which ends the Edge.
create function gnomon.edge_builtin_rules()
  returns table (property text, rule jsonpath)
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    values ('holds', 'strict $.type() == "boolean" && $ == true'::jsonpath)
  $$;

-- The merge rule a Kind had at a Recorded time: as `gnomon.kind_merge_rule`,
-- reading only the rule changes made by then.
create function gnomon.kind_merge_rule_at(p_tenant_id uuid, p_kind text, p_at timestamptz)
  returns text
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case when exists (
      select from gnomon.kind k
      cross join lateral unnest(k.ancestors || k.name) a(name)
      where k.tenant_id = p_tenant_id and k.name = p_kind
        and (select m.merge_rule from gnomon.kind_merge m
              where m.tenant_id = k.tenant_id and m.kind = a.name and m.set_at <= p_at
              order by m.id desc limit 1) = 'ranked'
    ) then 'ranked' else 'recency' end
  $$;

-- The reserved Edge Kinds every Ontology has. Each searches the Sentence, as
-- the root Edge Kind does, joins any Node to any Node, and is symmetric.
create function gnomon.reserved_kinds()
  returns table (
    name text,
    kind_of text,
    parent text,
    search text[],
    source_kinds text[],
    target_kinds text[],
    is_symmetric boolean
  )
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    values
      ('same_as', 'edge', 'edge', array['sentence'], array['node'], array['node'], true),
      ('different_from', 'edge', 'edge', array['sentence'], array['node'], array['node'], true)
  $$;

-- The Kinds a Node or an Edge of `p_kind` belongs to: the Kind and its
-- ancestors, with the reserved Kinds below the root Kind `edge`.
create function gnomon.kind_lineage(p_tenant_id uuid, p_kind text)
  returns text[]
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select coalesce(
      (select array[r.name] from gnomon.root_kinds() r where r.name = p_kind),
      (select array[r.parent, r.name] from gnomon.reserved_kinds() r where r.name = p_kind),
      (select k.ancestors || k.name from gnomon.kind k
       where k.tenant_id = p_tenant_id and k.name = p_kind)
    )
  $$;

create function gnomon.read_kind(p_tenant_id uuid, p_name text)
  returns table (
    name text,
    kind_of text,
    parent text,
    ancestors text[],
    properties jsonb,
    rules jsonb,
    search text[],
    source_kinds text[],
    target_kinds text[],
    is_symmetric boolean
  )
  language sql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
    select r.name, r.kind_of, null::text, '{}'::text[],
           '{"type": "object", "properties": {}, "required": []}'::jsonb, '{}'::jsonb,
           r.search, r.source_kinds, r.target_kinds, false
    from gnomon.root_kinds() r
    where r.name = p_name and p_tenant_id = gnomon.tenant_id()
    union all
    select r.name, r.kind_of, r.parent, array[r.parent],
           '{"type": "object", "properties": {}, "required": []}'::jsonb, '{}'::jsonb,
           r.search, r.source_kinds, r.target_kinds, r.is_symmetric
    from gnomon.reserved_kinds() r
    where r.name = p_name and p_tenant_id = gnomon.tenant_id()
    union all
    select k.name, k.kind_of, k.parent, k.ancestors, k.properties,
           coalesce((
             select jsonb_object_agg(x.property, x.rule::text)
             from gnomon.kind_rule x
             where x.tenant_id = k.tenant_id and x.kind = k.name
           ), '{}'::jsonb),
           k.search, k.source_kinds, k.target_kinds, false
    from gnomon.kind k
    where k.tenant_id = p_tenant_id and k.name = p_name
  $$;

-- Kind registration refuses a Kind of either name, of Nodes or of Edges,
-- whatever its definition. It inserts each Kind it registers, so this
-- trigger refuses the row before anything is written. The function runs as
-- the definer during registration, which holds EXECUTE on
-- `gnomon.reserved_kinds()`.
create function gnomon.kind_check_reserved_name() returns trigger
  language plpgsql security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    if exists (select from gnomon.reserved_kinds() r where r.name = new.name) then
      raise exception using errcode = 'GNK01',
        message = format(
          '"%s" is a reserved Edge Kind, which every Ontology already has; '
          'a Kind cannot take its name', new.name);
    end if;
    return new;
  end
  $$;

create trigger kind_check_reserved_name before insert on gnomon.kind
  for each row execute function gnomon.kind_check_reserved_name();

-- Whether an Edge of `p_kind` joins its Nodes in no direction.
create function gnomon.is_symmetric_kind(p_kind text)
  returns boolean
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select coalesce(
      (select r.is_symmetric from gnomon.reserved_kinds() r where r.name = p_kind), false)
  $$;

-- One Edge of a symmetric Kind per pair of Nodes in a Scope. The Scope is
-- part of the key, as every Edge lives in one Scope, so a caller's Edge never
-- collides with one in a Scope it cannot see.
create unique index edge_symmetric on gnomon.edge (tenant_id, scope, kind, source_id, target_id)
  where gnomon.is_symmetric_kind(kind);

insert into gnomon.schema_version (version, name) values (2, '0002_kinds');
