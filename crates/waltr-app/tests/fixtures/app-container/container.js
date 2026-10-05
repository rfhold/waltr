;(async function () {
  const output = document.getElementById('results')
  window.addEventListener('message', (event) => {
    if (event.source !== inner.contentWindow || event.data?.type !== 'waltr-ipc-probe') return
    output.textContent += JSON.stringify(event.data) + '\n'
  })
  const inner = document.createElement('iframe')
  inner.title = 'Opaque malicious inner app'
  inner.setAttribute('sandbox', 'allow-scripts')
  const source = await fetch('malicious-probe.js').then((response) => {
    if (!response.ok) throw new Error('fixture script unavailable')
    return response.text()
  })
  inner.srcdoc = '<!doctype html><meta charset="utf-8"><pre id="results"></pre><script>'
    + source.replaceAll('</script', '<\\/script') + '</script>'
  document.body.append(inner)
})().catch(() => {
  document.getElementById('results').textContent += 'Opaque fixture did not start; proof unavailable.\n'
})
