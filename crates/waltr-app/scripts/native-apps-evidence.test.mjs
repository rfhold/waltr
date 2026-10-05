import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

const run = input => spawnSync(process.execPath, [new URL('./native-apps-evidence.mjs', import.meta.url).pathname], { input, encoding: 'utf8' });

test('sanitizer requires explicit native denials and frame callbacks without exposing the real key', () => {
  const lines = ['WALTR_NATIVE_PROBE positive-control', 'WALTR_NATIVE_PROBE navigation-installed'];
  for (const frame of ['main', 'subframe', 'popup']) lines.push(`WALTR_NATIVE_PROBE navigation-denied.${frame}`);
  for (const frame of ['outer', 'inner']) for (let command = 0; command < 7; command++) for (const route of ['raw', 'ipc']) {
    lines.push(`__TAURI_INVOKE_KEY__ expected ACTUAL-SECRET-KEY but received waltr-probe.${frame}.${command}.${route}`);
  }
  const result = run(lines.join('\n'));
  assert.equal(result.status, 0);
  assert.match(result.stdout, /NATIVE KEY-FENCE CHECK PASS/);
  assert.equal(result.stdout.includes('ACTUAL-SECRET-KEY'), false);
  assert.equal(result.stderr, '');
});

test('timeouts, absence and arbitrary diagnostics never count as native proof', () => {
  const result = run('WALTR_NATIVE_PROBE positive-control\nfailed-or-timeout-not-proof\n__TAURI_INVOKE_KEY__ expected ACTUAL-SECRET-KEY but received unknown\n');
  assert.equal(result.status, 1);
  assert.match(result.stdout, /NATIVE PROOF INCOMPLETE/);
  assert.equal(result.stdout.includes('ACTUAL-SECRET-KEY'), false);
});
