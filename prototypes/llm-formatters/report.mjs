// PROTOTYPE 3 (throwaway): accuracy + token tables from results/*.jsonl
import fs from 'node:fs';
const FM = ['json', 'json-compact', 'markdown', 'toon', 'toon-flat'];
const ds = process.argv[3] ?? 'large';
const files = fs.readdirSync(new URL(`./results/${ds}/`, import.meta.url)).filter((f) => f.endsWith('.jsonl'));
const all = files.flatMap((f) => fs.readFileSync(new URL(`./results/${ds}/${f}`, import.meta.url), 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)));
const only = process.argv[2] === 'all' ? null : process.argv[2]; // optional model filter substring
const regime = process.argv[4]; // e.g. low-0 | none-1
const rows = all.filter((r) => (!only || r.model.includes(only)) && (!regime || `${r.effort}-${r.direct ? 1 : 0}` === regime));
const pct = (a) => (a.length ? (100 * a.filter((r) => r.ok).length / a.length).toFixed(1) : '-');
const mean = (a, k) => (a.length ? Math.round(a.reduce((s, r) => s + (r[k] ?? 0), 0) / a.length) : '-');
const models = [...new Set(rows.map((r) => r.model))];
for (const m of models) {
  const mr = rows.filter((r) => r.model === m);
  const qids = new Set(mr.map((r) => r.qid));
  console.log(`\n## ${m}  (${mr.length} calls, ${qids.size} questions, effort ${[...new Set(mr.map((r) => r.effort))]})`);
  console.log('| format | accuracy % | n | mean input tokens | mean output tokens | cost US$ |');
  console.log('|---|---|---|---|---|---|');
  for (const f of FM) { const a = mr.filter((r) => r.fmt === f); if (a.length) console.log(`| ${f} | ${pct(a)} | ${a.length} | ${mean(a, 'prompt_tokens')} | ${mean(a, 'completion_tokens')} | ${a.reduce((s, r) => s + (r.cost ?? 0), 0).toFixed(3)} |`); }
  const types = [...new Set(mr.map((r) => r.type))];
  console.log('\n| type (n) | ' + FM.join(' | ') + ' |'); console.log('|---|' + FM.map(() => '---|').join(''));
  for (const t of types) { const n = new Set(mr.filter((r) => r.type === t).map((r) => r.qid)).size; console.log(`| ${t} (${n}) | ` + FM.map((f) => pct(mr.filter((r) => r.type === t && r.fmt === f))).join(' | ') + ' |'); }
  const trunc = mr.filter((r) => r.extracted === null).length; if (trunc) console.log(`\nno ANSWER line: ${trunc} (finish: ${[...new Set(mr.filter((r) => r.extracted === null).map((r) => r.finish))]})`);
}
