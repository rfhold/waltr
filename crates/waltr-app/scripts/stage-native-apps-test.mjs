import { copyFile, mkdir } from 'node:fs/promises';
const root = new URL('../', import.meta.url);
const out = new URL('frontend-dist/', root);
await mkdir(out, { recursive: true });
for (const name of ['native-apps-test.html', 'native-apps-test.js']) {
  await copyFile(new URL(`tests/native-apps/${name}`, root), new URL(name, out));
}
