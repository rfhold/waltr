;(async () => {
  const status = document.getElementById('status')
  const results = document.getElementById('results')
  const invoke = window.__TAURI_INTERNALS__?.invoke
  try {
    if (window.top !== window || !invoke) throw new Error('no trusted transport')
    const started = await invoke('start_runtime')
    const bootstrap = started.nativeAppsTest
    if (!bootstrap) throw new Error('not a native test build')
    // A successful second command proves the trusted transport still works after startup.
    await invoke('client_state', { afterSequence: null })
    const frame = document.createElement('iframe')
    frame.title = 'Native IPC attacker: outer and opaque inner'
    frame.sandbox = 'allow-scripts allow-same-origin'
    frame.referrerPolicy = 'no-referrer'
    const url = new URL(bootstrap.probeUrl)
    if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.pathname !== '/probe/container.html') throw new Error('invalid fixture')
    frame.src = url.href
    window.addEventListener('message', event => {
      if (event.source !== frame.contentWindow || event.origin !== url.origin) return
      const report = event.data
      if (!report || report.type !== 'waltr-ipc-probe' || typeof report.attempt !== 'string' || typeof report.outcome !== 'string') return
      results.textContent += JSON.stringify(report).slice(0, 512) + '\n'
      if (/SUCCEEDED|LEAKED|EXPOSED/.test(report.outcome)) status.textContent = 'FAIL: forbidden frame capability reported. Do not enable production Apps.'
    })
    document.getElementById('frames').append(frame)
    status.textContent = 'POSITIVE CONTROL OK. Frame probes are running. Native proof is PENDING: inspect the sanitized native denial report in the launching terminal. Only explicit native rejections count; timeout/source absence do not.'
    const open = document.getElementById('open')
    open.disabled = false
    open.onclick = () => { window.location.href = 'index.html' }
    const navigation = document.getElementById('navigation')
    navigation.disabled = false
    navigation.onclick = () => {
      // Native callbacks must explicitly reject both; browser failures alone are not proof.
      window.open(url.href, '_blank')
      window.location.href = url.href
    }
  } catch {
    status.textContent = 'FAIL: trusted positive control or test bootstrap unavailable. Use the macOS native-apps-test feature and disposable configuration together.'
  }
})()
