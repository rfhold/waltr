;(async () => {
  const frame = window.origin === 'null' ? 'inner' : 'outer'
  const report = (attempt, outcome) => {
    const value = { type: 'waltr-ipc-probe', frame, attempt, outcome }
    document.getElementById('results').textContent += JSON.stringify(value) + '\n'
    window.parent.postMessage(value, '*')
  }
  const commands = ['start_runtime', 'client_dispatch', 'client_state', 'dispose_runtime',
    'open_external_url', 'open_authorization_attempt', 'plugin:__TAURI_CHANNEL__|fetch']
  for (const ancestor of [window.parent, window.top]) {
    try { report('ancestor-api', ancestor.__TAURI_INTERNALS__?.invoke ? 'EXPOSED' : 'absent-not-proof') }
    catch { report('ancestor-api', 'cross-origin-denied') }
  }
  let leaked
  for (const object of [window, window.__TAURI_INTERNALS__, window.ipc]) {
    if (!object) continue
    for (const name of Object.getOwnPropertyNames(object)) {
      const value = Object.getOwnPropertyDescriptor(object, name)?.value
      if (typeof value !== 'function') continue
      const key = /const\s+__TAURI_INVOKE_KEY__\s*=\s*["']([^"']+)/.exec(Function.prototype.toString.call(value))?.[1]
      if (key) leaked = key
    }
  }
  report('function-source-key', leaked ? 'LEAKED' : 'absent-not-proof')
  report('key-bearing-transport', window.__TAURI_INTERNALS__?.postMessage ? 'EXPOSED' : 'absent-not-proof')
  let callback = 920000
  for (let index = 0; index < commands.length; index++) {
    const cmd = commands[index]
    const raw = window.ipc?.postMessage?.bind(window.ipc)
      ?? window.webkit?.messageHandlers?.ipc?.postMessage?.bind(window.webkit.messageHandlers.ipc)
    for (const key of ['', `waltr-probe.${frame}.${index}.raw`, ...(leaked ? [leaked] : [])]) {
      try {
        if (!raw) throw new Error('no raw transport')
        raw(JSON.stringify({ cmd, callback: callback++, error: callback++, payload: {},
          options: { headers: { 'Tauri-Channel-Id': '0', Origin: 'tauri://localhost' } }, __TAURI_INVOKE_KEY__: key }))
        report(`raw:${cmd}`, 'sent-native-denial-required')
      } catch { report(`raw:${cmd}`, 'unavailable-not-proof') }
    }
    // Issue concurrently: invalid-key requests deliberately receive no callback from Tauri.
    for (const origin of ['ipc://localhost', 'http://ipc.localhost']) {
      const route = origin.startsWith('ipc:') ? 'ipc' : 'http'
      const controller = new AbortController()
      const timer = setTimeout(() => controller.abort(), 3000)
      void fetch(`${origin}/${encodeURIComponent(cmd)}`, { method: 'POST', body: '{}', signal: controller.signal,
        headers: { 'Content-Type': 'application/json', 'Tauri-Invoke-Key': `waltr-probe.${frame}.${index}.${route}`,
          'Tauri-Callback': String(callback++), 'Tauri-Error': String(callback++), 'Tauri-Channel-Id': '0' } })
        .then(response => report(`fetch:${route}:${cmd}`, response.headers.get('Tauri-Response') === 'ok' ? 'SUCCEEDED' : 'response-requires-review'))
        .catch(() => report(`fetch:${route}:${cmd}`, 'failed-or-timeout-not-proof'))
        .finally(() => clearTimeout(timer))
    }
  }
  report('submission-complete', 'await-independent-native-evidence')
  if (frame === 'outer') setTimeout(() => {
    report('subframe-navigation', 'await-independent-native-evidence')
    window.location.href = new URL('/probe/forbidden.html', window.location.href).href
  }, 6000)
})()
