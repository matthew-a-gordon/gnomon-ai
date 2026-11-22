// PROTOTYPE 3 (throwaway): synthetic bi-temporal graph + questions with computed answers.
// Run: node gen.mjs  -> writes dataset.json
import fs from 'node:fs';

function mulberry32(a) { return () => { a |= 0; a = a + 0x6D2B79F5 | 0; let t = Math.imul(a ^ a >>> 15, 1 | a); t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t; return ((t ^ t >>> 14) >>> 0) / 4294967296; }; }
const rnd = mulberry32(20261121);
const int = (a, b) => a + Math.floor(rnd() * (b - a + 1));
const pick = (a) => a[Math.floor(rnd() * a.length)];
const shuffle = (a) => { a = [...a]; for (let i = a.length - 1; i > 0; i--) { const j = Math.floor(rnd() * (i + 1)); [a[i], a[j]] = [a[j], a[i]]; } return a; };

// ---------- pools (drawn without replacement) ----------
const FIRST = ['Ada', 'Bram', 'Chloe', 'Dmitri', 'Elena', 'Farid', 'Greta', 'Hugo', 'Imani', 'Jonas', 'Keiko', 'Luca', 'Mira', 'Nikhil', 'Olga', 'Pavel', 'Quinn', 'Rosa', 'Soren', 'Tessa', 'Umar', 'Vera', 'Wen', 'Xavier', 'Yara', 'Zane'];
const LAST = ['Aldous', 'Brandt', 'Castell', 'Dufour', 'Eklund', 'Fontaine', 'Garrity', 'Holm', 'Ivanek', 'Jablonski', 'Kessler', 'Laurent', 'Moreau', 'Novak', 'Okafor', 'Petrov', 'Quist', 'Rinaldi', 'Sato', 'Thorne', 'Ulrich', 'Varga', 'Whitlock', 'Yilmaz'];
const ORG_A = ['Northwind', 'Halcyon', 'Ironbark', 'Juniper', 'Kestrel', 'Lumen', 'Meridian', 'Nimbus', 'Orchard', 'Pinnacle', 'Quarry', 'Redwood', 'Solstice', 'Tidewater', 'Umbra', 'Vantor'];
const ORG_B = ['Labs', 'Systems', 'Holdings', 'Works', 'Analytics', 'Dynamics', 'Partners', 'Group'];
const CITIES = ['Lisbon', 'Oslo', 'Kraków', 'Austin', 'Osaka', 'Nairobi', 'Lyon', 'Denver', 'Tartu', 'Porto', 'Leeds', 'Perth', 'Quito', 'Riga', 'Turin', 'Utrecht', 'Valencia', 'Wellington', 'Zagreb', 'Bergen', 'Cork', 'Dundee', 'Essen', 'Faro'];
const TITLES = ['Analyst', 'Senior Analyst', 'Engineer', 'Staff Engineer', 'Designer', 'Product Manager', 'Data Scientist', 'Account Executive', 'Operations Lead', 'Research Fellow', 'Support Specialist', 'Architect', 'Controller', 'Recruiter', 'Counsel', 'Technical Writer', 'Site Lead', 'Director of Sales'];
const bag = (arr) => { let s = shuffle(arr); return () => { if (!s.length) s = shuffle(arr); return s.pop(); }; };
let personNames, orgNames;
const refillNames = () => { personNames = shuffle(FIRST.flatMap((f) => LAST.map((l) => `${f} ${l}`))); orgNames = shuffle(ORG_A.flatMap((a) => ORG_B.map((b) => `${a} ${b}`))); };
refillNames();
const nextPerson = () => personNames.pop();
const nextOrg = () => orgNames.pop();
const nextCity = bag(CITIES);
const nextTitle = bag(TITLES);

// ---------- time helpers ----------
const pad = (n) => String(n).padStart(2, '0');
const D = (y, m, d = 1) => new Date(Date.UTC(y, m - 1, d));
const addMonths = (dt, n) => D(dt.getUTCFullYear(), dt.getUTCMonth() + 1 + n, dt.getUTCDate());
const addDays = (dt, n) => new Date(dt.getTime() + n * 86400000);
const iso = (dt) => dt.toISOString().slice(0, 10);
const instant = (dt, h = 9, mi = 30) => `${iso(dt)}T${pad(h)}:${pad(mi)}:00Z`;
function bound(dt, precision) {
  const y = dt.getUTCFullYear(), m = dt.getUTCMonth() + 1, d = dt.getUTCDate();
  if (precision === 'year') return { at: `${y}-01-01`, precision };
  if (precision === 'month') return { at: `${y}-${pad(m)}-01`, precision };
  if (precision === 'day') return { at: `${y}-${pad(m)}-${pad(d)}`, precision };
  if (precision === 'instant') return { at: `${y}-${pad(m)}-${pad(d)}T10:15:00Z`, precision };
  throw new Error(precision);
}
const NEG = { at: '-infinity', precision: 'unknown' };
const POS = { at: 'infinity', precision: 'unknown' };
const ms = (s) => (s === '-infinity' ? -Infinity : s === 'infinity' ? Infinity : Date.parse(s.length === 10 ? `${s}T00:00:00Z` : s));
function periodEnd(b) {
  const dt = new Date(ms(b.at));
  if (b.precision === 'year') return D(dt.getUTCFullYear() + 1, 1).getTime();
  if (b.precision === 'month') return addMonths(dt, 1).getTime();
  if (b.precision === 'day') return addDays(dt, 1).getTime();
  return null; // instant, unknown
}
// ADR-0008: uncertain when the valid-at instant falls inside the period of an imprecise bound
const isUncertain = (v, valid_at) => {
  const t = ms(valid_at);
  return [v.valid.from, v.valid.to].some((b) => { const e = periodEnd(b); return e !== null && t >= ms(b.at) && t < e; });
};
const visible = (v, valid_at, known_at) => {
  const t = ms(valid_at), k = ms(known_at);
  return ms(v.valid.from.at) <= t && t < ms(v.valid.to.at) && ms(v.recorded.from) <= k && k < ms(v.recorded.to);
};

// ---------- builders ----------
let vc, ec, nc, epc, rclock;
const resetCounters = () => { vc = 0; ec = 0; nc = 0; epc = 0; refillNames(); };
const ep = () => `ep${++epc}`;
const recAt = (y, m) => instant(D(y, m, int(1, 27)), int(7, 18), int(0, 59));
function ver(entity, props, vf, vt, rf, rt, citeProps) {
  const v = { id: `v${++vc}`, entity: entity.id, props, valid: { from: vf, to: vt }, recorded: { from: rf, to: rt ?? 'infinity' }, citations: (citeProps ?? [Object.keys(props).find((k) => k !== 'name')]).map((p) => ({ episode: ep(), property: p })) };
  entity.versions.push(v); return v;
}
const person = () => ({ id: `n${++nc}`, kind: 'Person', name: nextPerson(), versions: [] });
const org = () => ({ id: `n${++nc}`, kind: 'Org', name: nextOrg(), versions: [] });
const edge = (kind, from, to) => ({ id: `e${++ec}`, kind, from: from.id, to: to.id, versions: [] });

// a person with a clean title history (noise)
function noisePerson() {
  const p = person(); let s = D(int(2015, 2019), int(1, 12)); const n = int(2, 3);
  for (let i = 0; i < n; i++) {
    const e = addMonths(s, int(14, 40));
    ver(p, { name: p.name, title: nextTitle() }, bound(s, 'month'), i === n - 1 ? POS : bound(e, 'month'), recAt(2024, int(1, 4)));
    s = e;
  }
  return p;
}
const NOW = '2025-12-01T00:00:00Z';
const TAG = process.env.TAG ?? 'small'; const NOISE = TAG === 'large' ? [14, 24] : [3, 6];
function finish(ents, q) {
  for (let i = int(NOISE[0], NOISE[1]); i > 0; i--) ents.push(noisePerson());
  const entities = shuffle(ents).map((e) => ({ ...e, versions: [...e.versions].sort((a, b) => ms(a.valid.from.at) - ms(b.valid.from.at) || ms(a.recorded.from) - ms(b.recorded.from)) }));
  return { ...q, entities };
}
const titleAt = (p, V, K) => { const vs = p.versions.filter((v) => visible(v, V, K)); if (vs.length > 1) throw new Error('ambiguous ' + p.name); return vs[0]; };

// ---------- question types ----------
function qMultihop() { // A: employer's HQ city as-of a past valid-at (known-at now)
  const p = person(), o1 = org(), o2 = org(), e1 = edge('works_at', p, o1), e2 = edge('works_at', p, o2);
  const y1 = int(2015, 2018), S1 = D(y1, int(1, 12)), T1 = addMonths(S1, int(24, 40)), E1 = addMonths(T1, int(12, 30));
  const V = iso(addMonths(S1, Math.round((12 * (T1 - S1)) / (365.25 * 86400000) / 2)));
  ver(e1, { employment: 'full-time' }, bound(S1, 'month'), bound(E1, 'month'), recAt(2024, 2));
  ver(e2, { employment: 'full-time' }, bound(E1, 'month'), POS, recAt(2024, 3));
  const cA = nextCity(), cB = nextCity(), cD = nextCity(), cD2 = nextCity();
  if (rnd() < 0.5) { // a Correction on the relevant HQ value
    const wrong = nextCity(); const R1 = recAt(2024, 3), R2 = recAt(2024, 8);
    ver(o1, { name: o1.name, hq_city: wrong }, NEG, bound(T1, 'month'), R1, R2);
    ver(o1, { name: o1.name, hq_city: cA }, NEG, bound(T1, 'month'), R2, null);
  } else ver(o1, { name: o1.name, hq_city: cA }, NEG, bound(T1, 'month'), recAt(2024, 3));
  ver(o1, { name: o1.name, hq_city: cB }, bound(T1, 'month'), POS, recAt(2024, 4));
  ver(o2, { name: o2.name, hq_city: cD }, NEG, bound(addMonths(E1, 20), 'month'), recAt(2024, 4));
  ver(o2, { name: o2.name, hq_city: cD2 }, bound(addMonths(E1, 20), 'month'), POS, recAt(2024, 5));
  const p2 = person(), p3 = person(); const e3 = edge('works_at', p2, o1), e4 = edge('works_at', p3, o2);
  ver(e3, { employment: 'contract' }, bound(addMonths(S1, 3), 'month'), POS, recAt(2024, 4));
  ver(e4, { employment: 'full-time' }, bound(addMonths(S1, 5), 'month'), POS, recAt(2024, 4));
  ver(p, { name: p.name, title: nextTitle() }, bound(S1, 'month'), POS, recAt(2024, 2));
  const ents = [p, o1, o2, e1, e2, e3, e4, p2, p3];
  { const ev = [e1, e2].find((e) => e.versions.some((v) => visible(v, V, NOW))); const emp = ev.to === o1.id ? o1 : o2; const hv = emp.versions.filter((v) => visible(v, V, NOW)); if (hv.length !== 1 || hv[0].props.hq_city !== cA) throw new Error('multihop check'); }
  return finish(ents, { type: 'multihop', as_of: { valid_at: V, known_at: NOW }, question: `As of valid-at ${V} and known-at ${NOW}, in which city was the employer of ${p.name} headquartered? (Follow the works_at edge, then read that employer's hq_city at the same as-of.)`, answer: cA });
}

function titleTimeline(p, { gap = false, k = 3 } = {}) {
  let s = D(int(2016, 2019), int(1, 12)); const segs = [];
  for (let i = 0; i < k; i++) {
    const e = addMonths(s, int(14, 36)); const last = i === k - 1;
    const nxt = gap && i === 1 ? addMonths(e, 9) : e;
    segs.push({ title: nextTitle(), s, e: last ? null : e, next: nxt }); s = nxt;
  }
  return segs;
}
function qAsofValid() { // B
  const p = person(), gap = rnd() < 0.2; const segs = titleTimeline(p, { gap });
  const vs = segs.map((g, i) => ver(p, { name: p.name, title: g.title }, i === 0 ? bound(g.s, 'year') : bound(g.s, 'month'), g.e ? bound(g.e, 'month') : POS, recAt(2024, int(1, 5))));
  let V;
  if (gap) { const g = segs[1]; V = iso(addMonths(g.e, 4)); } else { const g = pick(segs.slice(0, 3)); const end = g.e ?? addMonths(g.s, 24); V = iso(new Date((g.s.getTime() + end.getTime()) / 2)); }
  const ents = [p, noisePerson(), noisePerson()];
  const hit = titleAt(p, V, NOW);
  return finish(ents, { type: 'asof_valid', as_of: { valid_at: V, known_at: NOW }, question: `As of valid-at ${V} and known-at ${NOW}, what was ${p.name}'s title? Answer "none" if no title was held.`, answer: hit ? hit.props.title : 'none' });
}

function qAsofKnown(variant) { // C
  const p = person(); const segs = titleTimeline(p); const si = int(0, 1); const s = segs[si];
  const R1 = recAt(2024, int(3, 5)), R2 = recAt(2024, int(9, 12));
  segs.forEach((g, i) => {
    const vf = i === 0 ? bound(g.s, 'year') : bound(g.s, 'month'), vt = g.e ? bound(g.e, 'month') : POS;
    if (i !== si) { ver(p, { name: p.name, title: g.title }, vf, vt, recAt(2024, int(1, 3))); return; }
    if (variant.startsWith('corr')) {
      const wrong = nextTitle(); ver(p, { name: p.name, title: wrong }, vf, vt, R1, R2); ver(p, { name: p.name, title: g.title }, vf, vt, R2, null);
    } else ver(p, { name: p.name, title: g.title }, vf, vt, R1, R2); // retracted
  });
  const V = iso(new Date((s.s.getTime() + s.e.getTime()) / 2));
  const mid = (a, b) => new Date((Date.parse(a) + Date.parse(b)) / 2).toISOString().replace(/\.\d+Z$/, 'Z');
  let K;
  if (variant.endsWith('before')) K = mid(R1, R2); else if (variant === 'corr_exact') K = R2; else K = instant(addMonths(D(2025, 1), int(0, 8)));
  const hit = titleAt(p, V, K);
  const ents = [p, noisePerson(), noisePerson()];
  return finish(ents, { type: 'asof_known', variant, as_of: { valid_at: V, known_at: K }, question: `As of valid-at ${V} and known-at ${K}, what was ${p.name}'s title as the system believed it at that known-at instant? Answer "none" if the system held no title for that instant.`, answer: hit ? hit.props.title : 'none' });
}

function qChanged(kind) { // D
  const p = person(); const ents = [p];
  const s = D(int(2016, 2019), int(1, 12));
  if (kind === 'correction') {
    const t0 = nextTitle(), wrong = nextTitle(), t2 = nextTitle(); const e = addMonths(s, int(14, 30)); const R1 = recAt(2024, 3), R2 = recAt(2024, 9);
    ver(p, { name: p.name, title: wrong }, bound(s, 'month'), bound(e, 'month'), R1, R2);
    ver(p, { name: p.name, title: t0 }, bound(s, 'month'), bound(e, 'month'), R2, null);
    ver(p, { name: p.name, title: t2 }, bound(e, 'month'), POS, recAt(2024, 4));
    const q = noisePerson(); const Rq1 = recAt(2024, 4), Rq2 = recAt(2024, 11); const qf = bound(D(2020, 2), 'month');
    ver(q, { name: q.name, title: nextTitle() }, qf, POS, Rq1, Rq2); ver(q, { name: q.name, title: nextTitle() }, qf, POS, Rq2, null);
    ents.push(q, noisePerson());
    return finish(ents, { type: 'changed_correction', as_of: null, question: `A Correction replaced an earlier recorded title for ${p.name}, which was wrong about the same valid time. What was the earlier, incorrect title?`, answer: wrong });
  }
  if (kind === 'end') {
    const T = nextTitle(), T2 = nextTitle(); const Dd = addDays(addMonths(s, int(20, 40)), int(0, 20)); const R1 = recAt(2024, 3), R2 = recAt(2024, 9);
    ver(p, { name: p.name, title: T }, bound(s, 'month'), POS, R1, R2);
    ver(p, { name: p.name, title: T }, bound(s, 'month'), bound(Dd, 'day'), R2, null);
    ver(p, { name: p.name, title: T2 }, bound(addMonths(Dd, 3), 'month'), POS, recAt(2025, 2));
    const q = noisePerson(); ents.push(q, noisePerson());
    return finish(ents, { type: 'changed_end', as_of: null, question: `${p.name} held the title "${T}" and then stopped holding it. On what date did that title stop being valid? Answer with the stored valid-to date as YYYY-MM-DD, as currently believed.`, answer: iso(Dd) });
  }
  if (kind === 'retraction') {
    const yes = rnd() < 0.45; const T = nextTitle(), U = nextTitle(); const e = addMonths(s, int(18, 30)); const R1 = recAt(2024, 3), R2 = recAt(2024, 10);
    ver(p, { name: p.name, title: U }, bound(s, 'month'), bound(e, 'month'), recAt(2024, 2));
    if (yes) ver(p, { name: p.name, title: T }, bound(e, 'month'), POS, R1, null, ['title', 'title']);
    else ver(p, { name: p.name, title: T }, bound(e, 'month'), POS, R1, R2);
    const q = noisePerson(); ents.push(q, noisePerson());
    return finish(ents, { type: 'changed_retraction', as_of: null, question: `As of known-at ${NOW}, does the system currently hold that ${p.name} had the title "${T}" at some point? Answer yes or no.`, answer: yes ? 'yes' : 'no' });
  }
  if (kind === 'refinement') {
    const T = nextTitle(); const R1 = recAt(2024, 3), R2 = recAt(2024, 9); const fromYear = rnd() < 0.5; const start = addMonths(D(int(2016, 2019), 1), int(1, 11));
    ver(p, { name: p.name, title: T }, fromYear ? bound(start, 'year') : NEG, POS, R1, R2);
    ver(p, { name: p.name, title: T }, bound(start, 'month'), POS, R2, null);
    ents.push(noisePerson(), noisePerson());
    return finish(ents, { type: 'changed_refinement', as_of: null, question: `${p.name}'s title "${T}" was first recorded with a less exact valid-from, and a later Refinement stated it more exactly. What is the stored valid-from date now, as YYYY-MM-DD?`, answer: iso(new Date(ms(bound(start, 'month').at))) });
  }
}

function buildUncertain(yes) {
  for (let tries = 0; tries < 50; tries++) {
    const { vc0, nc0, epc0 } = { vc0: vc, nc0: nc, epc0: epc };
    const p = person(); const k = 3; let s = D(int(2016, 2018), int(1, 12)); const bs = [];
    for (let i = 0; i <= k; i++) { bs.push({ dt: s, prec: pick(['month', 'year', 'day', 'instant', 'month']) }); s = addMonths(s, int(18, 34)); }
    const B = (i) => bound(bs[i].dt, bs[i].prec); const titles = Array.from({ length: k }, () => nextTitle());
    titles.forEach((t, i) => ver(p, { name: p.name, title: t }, B(i), i === k - 1 ? POS : B(i + 1), recAt(2024, int(1, 5))));
    const b1 = B(1), pe = periodEnd(b1); let V;
    if (yes) { if (pe === null || b1.precision === 'year' && false) continue; V = iso(new Date(ms(b1.at) + Math.floor((pe - ms(b1.at)) / 2 / 86400000) * 86400000)); }
    else V = iso(addMonths(bs[1].dt, int(5, 9)));
    // answer is the visible version at V; flag is its uncertain mark
    const hit = titleAt(p, V, NOW); if (!hit) continue;
    const u = isUncertain(hit, V); if (u !== yes) continue;
    return finish([p, noisePerson(), noisePerson()], { type: 'uncertain', as_of: { valid_at: V, known_at: NOW }, question: `For the as-of read at valid-at ${V} and known-at ${NOW}, is ${p.name}'s title in the result marked uncertain? Answer yes or no.`, answer: yes ? 'yes' : 'no' });
  }
  throw new Error('uncertain generation failed');
}

// ---------- assemble ----------
resetCounters();
const specs = [];
for (let i = 0; i < 15; i++) specs.push(() => qMultihop());
for (let i = 0; i < 15; i++) specs.push(() => qAsofValid());
['corr_before', 'corr_before', 'corr_before', 'corr_before', 'corr_before', 'corr_after', 'corr_after', 'corr_exact', 'corr_exact', 'retr_before', 'retr_before', 'retr_before', 'retr_before', 'retr_after', 'retr_after'].forEach((v) => specs.push(() => qAsofKnown(v)));
for (const [k, n] of [['correction', 8], ['end', 8], ['retraction', 7], ['refinement', 7]]) for (let i = 0; i < n; i++) specs.push(() => qChanged(k));
for (let i = 0; i < 15; i++) specs.push(() => buildUncertain(i < 8));

const questions = specs.map((f, i) => {
  resetCounters();
  const q = f(); q.id = `q${String(i + 1).padStart(2, '0')}`;
  // sanity: as-of answers recomputed from the rendered structure
  q.as_of && q.entities.forEach((e) => e.versions.forEach((v) => { v.uncertain = isUncertain(v, q.as_of.valid_at); }));
  if (!q.as_of) q.entities.forEach((e) => e.versions.forEach((v) => { delete v.uncertain; }));
  return q;
});
// retrieval header time marker used by the renderers
fs.writeFileSync(new URL(`./dataset-${TAG}.json`, import.meta.url), JSON.stringify({ now: NOW, questions }, null, 1));
const byType = {}; questions.forEach((q) => { byType[q.type] = (byType[q.type] || 0) + 1; });
const ans = {}; questions.forEach((q) => { ans[q.answer] = (ans[q.answer] || 0) + 1; });
console.log(questions.length, 'questions', byType);
console.log('avg versions per retrieval', (questions.reduce((s, q) => s + q.entities.reduce((a, e) => a + e.versions.length, 0), 0) / questions.length).toFixed(1));
console.log('yes/no/none:', ans.yes, ans.no, ans.none);
