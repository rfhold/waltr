;(async function () {
  const results = []
  const frame = window.origin === 'null' ? 'opaque-inner' : 'container'
  const record = (attempt, outcome) => {
    const report = { type: 'waltr-ipc-probe', frame, attempt, outcome }
    results.push(report)
    const output = document.getElementById('results')
    if (output) output.textContent += JSON.stringify(report) + '\n'
    if (window.parent !== window) window.parent.postMessage(report, '*')
  }
  const bounded = async (operation) => {
    let timer
    try {
      return await Promise.race([
        operation().then(() => 'SUCCEEDED'),
        new Promise((resolve) => { timer = setTimeout(() => resolve('timeout-not-proof'), 1500) })
      ])
    } catch { return 'rejected' } finally { clearTimeout(timer) }
  }
  const commands = ['start_runtime', 'client_dispatch', 'client_state', 'dispose_runtime',
    'open_external_url', 'open_authorization_attempt', 'plugin:__TAURI_CHANNEL__|fetch']
  const internals = window.__TAURI_INTERNALS__
  for (const target of [window.parent, window.top]) {
    try {
      record('ancestor-api', typeof target.__TAURI_INTERNALS__?.invoke === 'function'
        ? 'EXPOSED' : 'absent')
    } catch { record('ancestor-api', 'cross-origin-denied') }
  }
  let source = ''
  for (const object of [window, internals, window.__TAURI__, window.ipc]) {
    if (!object) continue
    for (const name of Object.getOwnPropertyNames(object)) {
      try {
        const descriptor = Object.getOwnPropertyDescriptor(object, name)
        const value = descriptor?.value
        if (typeof value === 'function') source += Function.prototype.toString.call(value)
        else if (typeof value === 'string') source += value
      } catch { /* A hostile getter is not needed for a source inspection. */ }
    }
  }
  // Never publish function sources, responses, credentials, or endpoint strings.
  const recovered = /const\s+__TAURI_INVOKE_KEY__\s*=\s*["']([^"']+)["']/.exec(source)?.[1]
  record('function-source-key', recovered ? 'LEAKED' : 'no-literal-found-not-proof')
  record('endpoint-scan', /https?:\/\/(?:127\.0\.0\.1|localhost):\d+/.test(source)
    ? 'REVIEW-loopback-string' : 'no-loopback-string-found-not-proof')
  record('bearer-scan', /Bearer\s+[A-Za-z0-9_-]{32,}/.test(source)
    ? 'LEAKED' : 'no-bearer-pattern-found-not-proof')
  record('key-bearing-transport', typeof internals?.postMessage === 'function' ? 'EXPOSED' : 'absent')
  for (const cmd of commands) {
    record(`invoke:${cmd}`, typeof internals?.invoke !== 'function' ? 'absent' :
      await bounded(() => internals.invoke(cmd, {}, { headers: { 'Tauri-Channel-Id': '0' } })))
    for (const key of ['', 'guessed', ...(recovered ? [recovered] : [])]) {
      const raw = { cmd, callback: 910001, error: 910002, payload: {},
        options: { headers: { Origin: 'tauri://localhost', 'Tauri-Channel-Id': '0' } },
        __TAURI_INVOKE_KEY__: key }
      try {
        if (!window.ipc?.postMessage) record(`raw:${cmd}`, 'absent')
        else {
          window.ipc.postMessage(JSON.stringify(raw))
          record(`raw:${cmd}`, 'sent-native-denial-must-be-observed')
        }
      } catch { record(`raw:${cmd}`, 'rejected') }
      for (const origin of ['ipc://localhost', 'http://ipc.localhost']) {
        record(`fetch:${cmd}`, await bounded(async () => {
          const response = await fetch(`${origin}/${encodeURIComponent(cmd)}`, {
            method: 'POST', body: '{}', headers: {
              'Content-Type': 'application/json', 'Tauri-Invoke-Key': key,
              'Tauri-Callback': '910001', 'Tauri-Error': '910002',
              'Tauri-Channel-Id': '0', Origin: 'tauri://localhost'
            }
          })
          if (!response.ok || response.headers.get('Tauri-Response') !== 'ok') throw new Error('denied')
        }))
      }
    }
  }
  // A harness may collect only these bounded, content-free outcomes.
  window.waltrIpcProbeResults = results
})()
