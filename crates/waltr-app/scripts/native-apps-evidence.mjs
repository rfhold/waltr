import { createInterface } from 'node:readline';

// Tauri's pinned native invalid-key diagnostic includes the real key. Never retain or print it.
const denials = new Set();
const navigation = new Set();
let positive = false;
for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  if (line === 'WALTR_NATIVE_PROBE positive-control') {
    positive = true;
    console.log('Native trusted positive control: OK');
  }
  const nativeNavigation = /^WALTR_NATIVE_PROBE (navigation-installed|navigation-denied\.(main|subframe|popup)|response-denied)$/.exec(line);
  if (nativeNavigation) {
    navigation.add(nativeNavigation[1]);
    console.log(`Native frame gate: ${nativeNavigation[1]}`);
  }
  const match = /^__TAURI_INVOKE_KEY__ expected .+ but received (waltr-probe\.(outer|inner)\.[0-6]\.(raw|ipc|http))$/.exec(line);
  if (match) {
    denials.add(match[1]);
    console.log(`Native invoke-key rejection: ${match[1]}`);
  }
}
const required = [];
for (const frame of ['outer', 'inner']) for (let command = 0; command < 7; command++) {
  // macOS uses the ipc: transport. http://ipc.localhost is additionally attempted but not a macOS protocol.
  for (const route of ['raw', 'ipc']) required.push(`waltr-probe.${frame}.${command}.${route}`);
}
const missing = required.filter(id => !denials.has(id));
const missingNavigation = ['navigation-installed', 'navigation-denied.main', 'navigation-denied.subframe', 'navigation-denied.popup'].filter(id => !navigation.has(id));
console.log(`Native report: positive=${positive}; observed=${required.length - missing.length}/${required.length} required denials`);
if (missing.length) console.log(`INCOMPLETE: ${missing.join(', ')}`);
if (missingNavigation.length) console.log(`INCOMPLETE frame gate: ${missingNavigation.join(', ')}`);
console.log(positive && !missing.length && !missingNavigation.length
  ? 'NATIVE KEY-FENCE CHECK PASS. Also inspect the page for ancestor/key leaks, popup/navigation denial and the real status App before considering any production gate change.'
  : 'NATIVE PROOF INCOMPLETE. Keep ordinary native Apps disabled. Timeouts and absent APIs do not count.');
process.exitCode = positive && !missing.length && !missingNavigation.length ? 0 : 1;
