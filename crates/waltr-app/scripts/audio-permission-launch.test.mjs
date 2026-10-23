import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const app = fileURLToPath(new URL('../', import.meta.url));
const javaRoot = join(app, 'gen/android/app/src/main/java/dev/waltr/app');

test('host JVM: launch SecurityException releases its ticket and permits a new grant', () => {
  const classes = mkdtempSync(join(tmpdir(), 'waltr-audio-permission-'));
  const run = (command, args) => {
    const result = spawnSync(command, args, { encoding: 'utf8' });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  };
  try {
    run('javac', [
      '--release', '8', '-d', classes,
      join(javaRoot, 'RecordAudioPermissionLaunch.java'),
      join(app, 'tests/android-permissions/RecordAudioPermissionLaunchTest.java'),
    ]);
    run('java', ['-cp', classes, 'dev.waltr.app.RecordAudioPermissionLaunchTest']);
  } finally {
    rmSync(classes, { recursive: true, force: true });
  }
});

test('Kotlin static contract: only launch uses the helper and failure clears the exact ticket', () => {
  const activity = readFileSync(join(javaRoot, 'MainActivity.kt'), 'utf8');
  assert.equal(activity.split('RecordAudioPermissionLaunch.run(').length - 1, 1);
  assert.ok(activity.includes(`      RecordAudioPermissionLaunch.run({
        recordAudioPermission.launch(Manifest.permission.RECORD_AUDIO)
      }, {
        if (audioTicket == ticket) {
          audioTicket = null
          nativeRecordAudioPermissionResult(ticket, false)
        }
      })`));
  assert.ok(activity.includes('savedInstanceState.getString("waltr.audioPermissionProcess") == audioProcessIdentity'));
  assert.ok(activity.includes('} else -1L'));
});
