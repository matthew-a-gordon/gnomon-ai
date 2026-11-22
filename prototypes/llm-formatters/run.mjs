// PROTOTYPE 3 (throwaway): run questions x formats against a model through OpenRouter.
// Usage: set -a; . ~/.config/gnomon-prototypes/env; set +a; node run.mjs --model anthropic/claude-haiku-5.5 [--formats a,b] [--limit N] [--qids q01,q02] [--effort low] [--max-tokens N] [--trial 1] [--conc 8]
import fs from 'node:fs';
import { FORMATS, SYSTEM, SYSTEM_DIRECT, userPrompt } from './render.mjs';

const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i < 0 ? d : process.argv[i + 1]; };
const dataset = arg('dataset', 'small'); const model = arg('model'); const effort = arg('effort', 'low'); const maxTokens = +arg('max-tokens', 4000);
const direct = arg('direct', '0') === '1'; const trial = +arg('trial', 1); const conc = +arg('conc', 8); const limit = +arg('limit', 1e9);
const formats = arg('formats', Object.keys(FORMATS).join(',')).split(',');
const only = arg('qids', null)?.split(',');
const key = process.env.OPENROUTER_API_KEY; if (!key) { console.error('OPENROUTER_API_KEY missing'); process.exit(2); }
const { questions } = JSON.parse(fs.readFileSync(new URL(`./dataset-${dataset}.json`, import.meta.url)));
const qs = questions.filter((q) => !only || only.includes(q.id)).slice(0, limit);
const file = new URL(`./results/${dataset}/${model.replace('/', '__')}.jsonl`, import.meta.url);
const done = new Set(fs.existsSync(file) ? fs.readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map((l) => { const r = JSON.parse(l); return `${r.qid}|${r.fmt}|${r.trial}|${r.effort}|${r.direct ? 1 : 0}`; }) : []);

const norm = (s) => s.toLowerCase().replace(/[*`"'“”]/g, '').replace(/\.$/, '').replace(/\s+/g, ' ').trim();
export function grade(text, answer) {
  const m = [...text.matchAll(/ANSWER:\s*(.+)/gi)].pop(); if (!m) return { extracted: null, ok: false };
  const ex = norm(m[1]); let a = norm(answer);
  const ok = ex === a || (a === 'none' && ['none', 'no title', 'null', 'n/a'].includes(ex));
  return { extracted: ex, ok };
}

async function call(q, fmt) {
  const body = { model, max_tokens: maxTokens, usage: { include: true }, messages: [{ role: 'system', content: direct ? SYSTEM_DIRECT : SYSTEM }, { role: 'user', content: userPrompt(fmt, q) }] };
  if (effort !== 'default') body.reasoning = { effort };
  for (let attempt = 0; attempt < 6; attempt++) {
    try {
      const res = await fetch('https://openrouter.ai/api/v1/chat/completions', { method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' }, body: JSON.stringify(body), signal: AbortSignal.timeout(120000) });
      if (res.status === 429 || res.status >= 500) { await new Promise((r) => setTimeout(r, 2000 * 2 ** attempt)); continue; }
      const j = await res.json();
      if (!res.ok || j.error) throw new Error(`HTTP ${res.status}: ${JSON.stringify(j.error ?? j).slice(0, 300)}`);
      return j;
    } catch (e) { if (attempt === 5) throw e; await new Promise((r) => setTimeout(r, 2000 * 2 ** attempt)); }
  }
}

const jobs = qs.flatMap((q) => formats.map((fmt) => ({ q, fmt }))).filter(({ q, fmt }) => !done.has(`${q.id}|${fmt}|${trial}|${effort}|${direct ? 1 : 0}`));
console.log(`${model}: ${jobs.length} calls to run (${done.size} cached)`);
let next = 0, spent = 0, errors = 0, finished = 0;
async function worker() {
  while (next < jobs.length) {
    const { q, fmt } = jobs[next++];
    try {
      const j = await call(q, fmt); const text = j.choices?.[0]?.message?.content ?? ''; const u = j.usage ?? {};
      const g = grade(text, q.answer); spent += u.cost ?? 0;
      const rec = { qid: q.id, type: q.type, fmt, model, trial, effort, direct, dataset, answer: q.answer, extracted: g.extracted, ok: g.ok, prompt_tokens: u.prompt_tokens, completion_tokens: u.completion_tokens, reasoning_tokens: u.completion_tokens_details?.reasoning_tokens ?? 0, cost: u.cost ?? null, finish: j.choices?.[0]?.finish_reason, text };
      fs.appendFileSync(file, JSON.stringify(rec) + '\n'); if (++finished % 25 === 0) console.log(`${finished}/${jobs.length} ~$${spent.toFixed(3)}`);
    } catch (e) { errors++; console.error(`FAIL ${q.id} ${fmt}: ${String(e.message).replace(key, '***')}`); }
  }
}
await Promise.all(Array.from({ length: conc }, worker));
console.log(`done. spent ~$${spent.toFixed(4)} (reported by OpenRouter), errors ${errors}`);
