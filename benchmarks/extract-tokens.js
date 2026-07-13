#!/usr/bin/env node
// Normalize per-task token usage across three agent runtimes into one schema.
// Each runtime reports usage differently; this collapses them to:
//   input (fresh, non-cached) / cache_write / cache_read / output / total / cost
// where total = input + cache_write + cache_read + output (matches Cursor's
// admin-report "Total Tokens" definition, verified against a sample export).
//
// Usage:
//   extract-tokens.js <runtime> <rawFile> <tokenlog|-> <taskId> <taskName> \
//                     <modelSlug> <canon> <startedAt> <endedAt> <wallMs> <outFile>
//
//   runtime  = daimonos | claude | cursor
//   rawFile  = claude/cursor stream-json (.jsonl) or daimonos stdout (.txt)
//   tokenlog = daimonos --debug-tokens delta (new lines only), or "-" otherwise

const fs = require('fs');

const [runtime, rawFile, tokenlog, taskId, taskName, modelSlug, canon,
  startedAt, endedAt, wallMsStr, outFile] = process.argv.slice(2);
const wallMs = parseInt(wallMsStr, 10) || 0;

function readLines(path) {
  try {
    return fs.readFileSync(path, 'utf8').split('\n');
  } catch (e) {
    return [];
  }
}

function jsonEvents(path) {
  const out = [];
  for (const line of readLines(path)) {
    const t = line.trim();
    if (!t) continue;
    try { out.push(JSON.parse(t)); } catch (e) { /* not a json line */ }
  }
  return out;
}

// Count assistant tool_use blocks in a stream-json transcript (claude/cursor).
function countToolCalls(events) {
  let n = 0;
  for (const ev of events) {
    if (ev.type !== 'assistant') continue;
    const content = (ev.message || {}).content || [];
    if (Array.isArray(content)) {
      for (const b of content) if (b.type === 'tool_use') n++;
    }
  }
  return n;
}

let m = { input: 0, cache_write: 0, cache_read: 0, output: 0, cost: 0 };
let toolCalls = 0;
let isError = true;
let cost = null; // null = unknown (Cursor: comes from admin CSV later)

if (runtime === 'claude') {
  const events = jsonEvents(rawFile);
  const result = events.find((e) => e.type === 'result') || null;
  const u = result ? (result.usage || {}) : {};
  m.input = u.input_tokens || 0;
  m.cache_write = u.cache_creation_input_tokens || 0;
  m.cache_read = u.cache_read_input_tokens || 0;
  m.output = u.output_tokens || 0;
  cost = result ? (result.total_cost_usd || 0) : 0;
  toolCalls = countToolCalls(events);
  isError = result ? (result.is_error !== undefined ? result.is_error : true) : true;
} else if (runtime === 'cursor') {
  const events = jsonEvents(rawFile);
  const result = events.find((e) => e.type === 'result') || null;
  const u = result ? (result.usage || {}) : {};
  m.input = u.inputTokens || 0;
  m.cache_write = u.cacheWriteTokens || 0;
  m.cache_read = u.cacheReadTokens || 0;
  m.output = u.outputTokens || 0;
  cost = null; // cursor-agent does not emit cost; joined from admin CSV
  toolCalls = countToolCalls(events);
  isError = result ? (result.is_error !== undefined ? result.is_error : true) : true;
} else if (runtime === 'daimonos') {
  // Sum per-LLM-call lines from the --debug-tokens delta; skip non-call
  // event lines (e.g. compaction) which carry no input/output token fields.
  let calls = 0;
  let sawLine = false;
  for (const ev of jsonEvents(tokenlog)) {
    if (ev.event) continue; // compaction / other structured events
    if (typeof ev.input !== 'number' && typeof ev.output !== 'number') continue;
    m.input += ev.input || 0;
    m.cache_write += ev.cache_write || 0;
    m.cache_read += ev.cache_read || 0;
    m.output += ev.output || 0;
    // cost_usd is logged as a fixed-decimal STRING (agent.rs), so coerce.
    m.cost += parseFloat(ev.cost_usd) || 0;
    calls++;
    sawLine = true;
  }
  cost = m.cost; // OpenRouter path often reports 0; tokens are primary
  toolCalls = calls > 0 ? calls - 1 : 0; // calls ~= turns; last is the answer turn
  // daimonos exits 0 on success; the runner records is_error via exit code in
  // meta, but if we got at least one token line the call reached the model.
  isError = !sawLine;
} else {
  console.error('unknown runtime: ' + runtime);
  process.exit(2);
}

const total = m.input + m.cache_write + m.cache_read + m.output;

const summary = {
  task_id: taskId,
  task_name: taskName,
  runtime: runtime,
  canon_model: canon,
  model_slug: modelSlug,
  started_at: startedAt,
  ended_at: endedAt,
  wall_ms: wallMs,
  input: m.input,
  cache_write: m.cache_write,
  cache_read: m.cache_read,
  output: m.output,
  total_tokens: total,
  cost_usd: cost,
  tool_calls: toolCalls,
  is_error: isError,
  success: !isError, // upgraded to correctness-gated by check-task.js
};

fs.writeFileSync(outFile, JSON.stringify(summary, null, 2));

const costStr = cost === null ? 'n/a (csv)' : ('$' + Number(cost).toFixed(4));
console.log('       tokens: ' + total.toLocaleString() +
  ' (in:' + m.input.toLocaleString() +
  ' cw:' + m.cache_write.toLocaleString() +
  ' cr:' + m.cache_read.toLocaleString() +
  ' out:' + m.output.toLocaleString() + ')' +
  ' | tools:' + toolCalls +
  ' | cost:' + costStr +
  ' | wall:' + wallMs.toLocaleString() + 'ms');
