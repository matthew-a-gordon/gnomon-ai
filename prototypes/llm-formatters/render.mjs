// PROTOTYPE 3 (throwaway): render one retrieval in each format from the same structured data.
import { encode } from '@toon-format/toon';

// canonical nested shape: entities with their Versions
export function structure(q) {
  const r = {};
  if (q.as_of) r.as_of = q.as_of;
  r.entities = q.entities.map((e) => {
    const out = { id: e.id, kind: e.kind };
    if (e.from) { out.from = e.from; out.to = e.to; }
    out.versions = e.versions.map(({ entity, ...v }) => v);
    return out;
  });
  return r;
}

// flat shape for TOON: one table per Kind, scalar columns only
export function flat(q) {
  const r = {};
  if (q.as_of) r.as_of = q.as_of;
  const byKind = {};
  for (const e of q.entities) for (const v of e.versions) {
    const row = { entity: e.id };
    if (e.from) { row.from = e.from; row.to = e.to; }
    row.version = v.id;
    Object.assign(row, v.props);
    row.valid_from = v.valid.from.at; row.valid_from_precision = v.valid.from.precision;
    row.valid_to = v.valid.to.at; row.valid_to_precision = v.valid.to.precision;
    row.recorded_from = v.recorded.from; row.recorded_to = v.recorded.to;
    if (v.uncertain !== undefined) row.uncertain = v.uncertain;
    row.citations = v.citations.map((c) => `${c.episode}:${c.property}`).join(' ');
    (byKind[e.kind] ??= []).push(row);
  }
  return { ...r, ...byKind };
}

const cell = (s) => String(s).replace(/\|/g, '\\|');
function markdown(q) {
  const out = [];
  if (q.as_of) out.push(`As-of read: valid-at ${q.as_of.valid_at}, known-at ${q.as_of.known_at}`, '');
  for (const e of q.entities) {
    out.push(`### ${e.kind} ${e.id}${e.from ? ` (${e.from} -> ${e.to})` : ''}`, '');
    const keys = [...new Set(e.versions.flatMap((v) => Object.keys(v.props)))];
    const hasU = e.versions.some((v) => v.uncertain !== undefined);
    out.push(`| version | ${keys.join(' | ')} | valid from | valid to | recorded from | recorded to |${hasU ? ' uncertain |' : ''} citations |`);
    out.push(`|${'---|'.repeat(keys.length + 6 + (hasU ? 1 : 0))}`);
    for (const v of e.versions) {
      const vb = (b) => (b.precision === 'unknown' ? `${b.at} (unknown)` : `${b.at} (${b.precision})`);
      out.push(`| ${v.id} | ${keys.map((k) => cell(v.props[k] ?? '')).join(' | ')} | ${vb(v.valid.from)} | ${vb(v.valid.to)} | ${v.recorded.from} | ${v.recorded.to} |${hasU ? ` ${v.uncertain ? 'yes' : 'no'} |` : ''} ${v.citations.map((c) => `${c.episode} (${c.property})`).join(', ')} |`);
    }
    out.push('');
  }
  return out.join('\n').trimEnd();
}

export const FORMATS = {
  json: (q) => JSON.stringify(structure(q), null, 2),
  'json-compact': (q) => JSON.stringify(structure(q)),
  markdown: (q) => markdown(q),
  toon: (q) => encode(structure(q)),
  'toon-flat': (q) => encode(flat(q)),
};

export const SYSTEM = `You answer questions from retrieval context taken from a bi-temporal knowledge graph. Use only the context.

Definitions:
- A Version is one immutable state of a Node or an Edge. An Edge joins two Nodes.
- Valid time [from, to) is when the Version was true in the world. Recorded time [from, to) is when the system held the Version as its current belief. Both are half-open: the start is included and the end is excluded.
- "infinity" as an end means open. "-infinity" as a valid start means the start is unknown and reads as open.
- Each valid-time bound has a precision: unknown, year, month, day or instant. The stored date is the start of that period, so "2021-03-01" with precision month means "in March 2021".
- A read "as of" a valid-at instant and a known-at instant returns the Versions whose Valid time contains valid-at and whose Recorded time contains known-at.
- A Correction is a new Version that replaces an earlier one because the earlier one was wrong about the same Valid time. An End of validity is a new Version stating that what an earlier one said was true and stopped being true. A Retraction is the finding that what a Version stated was never true: its Recorded time ends and nothing replaces it. A Refinement is a new Version that states a Valid-time bound the earlier one left less exact, changing nothing else.
- "uncertain" is true when the as-of valid-at instant falls inside the period of an imprecise (year, month or day) bound of that Version.
- Citations link a Version to the Episodes it was derived from.

Reply with a brief justification, then end with one final line in exactly this form: ANSWER: <answer>
Dates as YYYY-MM-DD. Yes/no questions: yes or no. If nothing applies: none.`;

export const SYSTEM_DIRECT = SYSTEM.replace('Reply with a brief justification, then end with one final line in exactly this form: ANSWER: <answer>', 'Reply with only one line in exactly this form, with no explanation: ANSWER: <answer>');

export const userPrompt = (fmt, q) => `<context format="${fmt}">\n${FORMATS[fmt](q)}\n</context>\n\nQuestion: ${q.question}`;
