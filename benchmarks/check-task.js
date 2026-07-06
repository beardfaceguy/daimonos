#!/usr/bin/env node
// Per-task correctness gate (#929): evaluate a task's machine-checkable
// `checks` against the agent's final response and the workspace it left
// behind, then stamp checks_passed / checks_total / correct into the task's
// summary json. "The agent didn't crash" is not the same as "the agent did
// the task" — savings measured on failed runs must not count.
//
// Usage: node check-task.js <task.json> <raw.jsonl> <workspace-dir> <summary.json>
//
// Check shapes (in the task json's "checks" array):
//   {"type": "response", "all": ["pat", ...]}            every regex matches (case-insensitive)
//   {"type": "response", "any": ["pat", ...], "min": 2}  at least min regexes match
//   {"type": "workspace", "command": "sh command"}       exit 0 in the workspace = pass

const fs = require('fs');
const { execSync } = require('child_process');

const [taskFile, rawFile, workspace, summaryFile] = process.argv.slice(2);
if (!summaryFile) {
  console.error('usage: check-task.js <task.json> <raw.jsonl> <workspace> <summary.json>');
  process.exit(2);
}

const task = JSON.parse(fs.readFileSync(taskFile, 'utf8'));
const checks = task.checks || [];

// The final response text lives in the stream's `result` event; fall back to
// concatenated assistant text blocks for streams that lack one.
function finalText() {
  let lines;
  try {
    lines = fs.readFileSync(rawFile, 'utf8').split('\n');
  } catch (e) {
    return '';
  }
  let assistantText = '';
  for (const line of lines) {
    if (!line.trim()) continue;
    let ev;
    try { ev = JSON.parse(line); } catch (e) { continue; }
    if (ev.type === 'result' && typeof ev.result === 'string') return ev.result;
    if (ev.type === 'assistant') {
      const content = (ev.message || {}).content || [];
      for (const block of content) {
        if (block.type === 'text' && block.text) assistantText += block.text + '\n';
      }
    }
  }
  return assistantText;
}

function runCheck(check, text) {
  if (check.type === 'response') {
    // Patterns are authored in this repo's task JSONs, not user input.
    // nosemgrep: javascript.lang.security.audit.detect-non-literal-regexp.detect-non-literal-regexp
    const matches = (pat) => new RegExp(pat, 'i').test(text);
    if (check.all) return check.all.every(matches);
    if (check.any) return check.any.filter(matches).length >= (check.min || 1);
    return false;
  }
  if (check.type === 'workspace') {
    try {
      // Executing a configured shell command IS the feature here: workspace
      // checks are commands like `grep -q display_names src/config.rs` that
      // assert filesystem ground truth. `check.command` is not user input —
      // it comes from the version-controlled task JSONs in this repo and the
      // harness is developer-invoked, not a service.
      // nosemgrep: javascript.lang.security.detect-child-process.detect-child-process
      execSync(check.command, { cwd: workspace, stdio: 'ignore', timeout: 120000, shell: '/bin/sh' });
      return true;
    } catch (e) {
      return false;
    }
  }
  console.error(`unknown check type: ${check.type}`);
  return false;
}

const text = finalText();
let passed = 0;
for (const check of checks) {
  if (runCheck(check, text)) passed++;
}

const summary = JSON.parse(fs.readFileSync(summaryFile, 'utf8'));
summary.checks_total = checks.length;
summary.checks_passed = passed;
summary.correct = checks.length === 0 ? null : passed === checks.length;
fs.writeFileSync(summaryFile, JSON.stringify(summary, null, 2));

const verdict = summary.correct === null ? 'no checks' : (summary.correct ? 'correct' : 'INCORRECT');
console.log(`       checks: ${passed}/${checks.length} (${verdict})`);
