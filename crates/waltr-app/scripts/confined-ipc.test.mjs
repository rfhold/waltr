import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import vm from 'node:vm'
import { webcrypto } from 'node:crypto'
import { join } from 'node:path'

const template = readFileSync(new URL('./confined-ipc.js', import.meta.url), 'utf8')
const serializer = readFileSync(new URL('./process-ipc-message-fn.js', import.meta.url), 'utf8')
const key = 'fixture-only-key-not-native'
const script = (os) => template.replace('__WALTR_OS_NAME__', JSON.stringify(os))
  .replace('__WALTR_PROCESS_IPC_MESSAGE_FN__', serializer).replace('__INVOKE_KEY__', JSON.stringify(key))

function realm({ os = 'macos', url = 'tauri://localhost/index.html', frame = false, fail = false,
  contentType = 'application/json', ok = true } = {}) {
  const requests = [], raw = [], callbacks = []
  const window = {
    location: new URL(url),
    __TAURI_INTERNALS__: {
      convertFileSrc: (cmd) => `ipc://localhost/${encodeURIComponent(cmd)}`,
      runCallback: (...args) => callbacks.push(args)
    },
    ipc: { postMessage: (message) => raw.push(JSON.parse(message)) }
  }
  window.top = frame ? {} : window
  const context = vm.createContext({ window, Headers, console: { warn() {} },
    fetch: async (url, options) => {
      requests.push({ url, ...options })
      if (fail) throw new Error('blocked custom protocol')
      return new Response(contentType === 'application/json' ? '{"result":true}' : 'body', {
        headers: { 'content-type': contentType, 'Tauri-Response': ok ? 'ok' : 'error' }
      })
    }
  })
  vm.runInContext(script(os), context)
  return { window, context, requests, raw, callbacks }
}
const settle = () => new Promise((resolve) => setImmediate(resolve))
const message = { cmd: 'client_state', callback: 7, error: 8, payload: { afterSequence: null } }

test('only exact platform-packaged top origins install key-bearing transport', () => {
  for (const [os, url] of [['macos', 'tauri://localhost/index.html'], ['linux', 'tauri://localhost/'],
    ['windows', 'http://tauri.localhost/index.html']]) {
    const { window } = realm({ os, url })
    assert.equal(typeof window.__TAURI_INTERNALS__.postMessage, 'function')
    assert.equal(window.__TAURI_INTERNALS__.postMessage.toString().includes(key), false)
    assert.equal(JSON.stringify(window.__TAURI_INTERNALS__).includes(key), false)
  }
  for (const url of ['tauri://localhost:123/', 'tauri://evil/', 'asset://localhost/',
    'http://tauri.localhost:123/', 'https://tauri.localhost/', 'http://localhost:5173/',
    'http://127.0.0.1:9876/apps/container', 'data:text/html,opaque', 'about:blank']) {
    for (const os of ['macos', 'linux', 'windows', 'android', 'unknown']) {
      assert.equal(realm({ os, url }).window.__TAURI_INTERNALS__.postMessage, undefined)
    }
  }
})

test('app container and opaque frames cannot add the key to raw IPC or channel fetch', async () => {
  for (const os of ['macos', 'linux', 'windows']) {
    for (const url of ['tauri://localhost/', 'http://tauri.localhost/',
      'http://127.0.0.1:9876/container', 'about:srcdoc']) {
      const { window, context, raw, requests } = realm({ os, url, frame: true })
      assert.equal(window.__TAURI_INTERNALS__.postMessage, undefined)
      window.ipc.postMessage(JSON.stringify({ ...message, __TAURI_INVOKE_KEY__: '' }))
      assert.equal(raw[0].__TAURI_INVOKE_KEY__, '')
      vm.runInContext(`fetch('ipc://localhost/plugin%3A__TAURI_CHANNEL__%7Cfetch', {
        method: 'POST', body: 'null', headers: new Headers({ 'Tauri-Channel-Id': '42' })
      });`, context)
      assert.equal(requests[0].headers.get('Tauri-Invoke-Key'), null)
      // Native rejection is covered separately by the Rust MockRuntime on_message test.
    }
  }
  await settle()
})

test('trusted transport preserves fetch headers, payload and success callback', async () => {
  const { window, requests, callbacks } = realm()
  window.__TAURI_INTERNALS__.postMessage({ ...message, options: { headers: { 'x-custom': 'yes',
    'Tauri-Invoke-Key': 'attacker-supplied' } } })
  await settle()
  assert.equal(requests[0].headers.get('Tauri-Invoke-Key'), key)
  assert.equal(requests[0].headers.get('x-custom'), 'yes')
  assert.equal(requests[0].headers.get('Tauri-Callback'), '7')
  assert.deepEqual(JSON.parse(requests[0].body), message.payload)
  assert.deepEqual(callbacks, [[7, { result: true }]])
})

test('trusted transport preserves text/error and binary response callbacks', async () => {
  for (const contentType of ['text/plain', 'application/octet-stream']) {
    const { window, callbacks } = realm({ contentType, ok: false })
    window.__TAURI_INTERNALS__.postMessage(message)
    await settle()
    assert.equal(callbacks[0][0], 8)
    assert.equal(contentType === 'text/plain' ? callbacks[0][1] :
      new TextDecoder().decode(callbacks[0][1]), 'body')
  }
})

test('custom protocol failure retries using private-key postMessage and stays on fallback', async () => {
  const { window, requests, raw } = realm({ fail: true })
  window.__TAURI_INTERNALS__.postMessage(message)
  await settle()
  assert.equal(raw[0].__TAURI_INVOKE_KEY__, key)
  assert.equal(raw[0].options.customProtocolIpcBlocked, true)
  assert.deepEqual(raw[0].payload, message.payload)
  window.__TAURI_INTERNALS__.postMessage(message)
  assert.equal(requests.length, 1)
  assert.equal(raw.length, 2)
})

test('channel fetch uses the same confined key and serialization retains stock special cases', async () => {
  const { window, context, requests } = realm()
  window.__TAURI_INTERNALS__.postMessage({ ...message, cmd: 'plugin:__TAURI_CHANNEL__|fetch',
    payload: null, options: { headers: { 'Tauri-Channel-Id': '42' } } })
  assert.equal(requests[0].headers.get('Tauri-Invoke-Key'), key)
  assert.equal(requests[0].headers.get('Tauri-Channel-Id'), '42')
  vm.runInContext(`window.__TAURI_INTERNALS__.postMessage({cmd: 'binary', payload: new Uint8Array([1,2])});
    window.__TAURI_INTERNALS__.postMessage({cmd: 'json', payload: {
      map: new Map([['a', 1]]), bytes: new Uint8Array([3,4]),
      channel: { __TAURI_TO_IPC_KEY__() { return 'channel:42' } }
    }});`, context)
  assert.equal(requests[1].headers.get('Content-Type'), 'application/octet-stream')
  assert.deepEqual(Array.from(requests[1].body), [1, 2])
  assert.deepEqual(JSON.parse(requests[2].body), { map: { a: 1 }, bytes: [3, 4], channel: 'channel:42' })
  await settle()
})

test('pinned upstream core invoke API works through confined transport and fallback',
  { skip: !process.env.WALTR_TAURI_SOURCE }, async () => {
    const root = process.env.WALTR_TAURI_SOURCE
    assert.match(readFileSync(join(root, 'Cargo.toml'), 'utf8'), /version = "2\.10\.1"/)
    const core = readFileSync(join(root, 'scripts/core.js'), 'utf8')
      .replace('__TEMPLATE_os_name__', '"macos"').replace('__TEMPLATE_protocol_scheme__', '"http"')
    const ipc = readFileSync(join(root, 'scripts/ipc.js'), 'utf8')
      .replace('__TEMPLATE_isolation_origin__', 'null')
    for (const fail of [false, true]) {
      const { window, context, raw } = realm({ fail })
      delete window.__TAURI_INTERNALS__.convertFileSrc
      delete window.__TAURI_INTERNALS__.runCallback
      window.__TAURI_INTERNALS__.__TAURI_PATTERN__ = { pattern: 'brownfield' }
      window.crypto = webcrypto
      vm.runInContext(core, context)
      vm.runInContext(ipc, context)
      const promise = window.__TAURI_INTERNALS__.invoke('client_state', message.payload)
      await settle()
      if (fail) {
        assert.equal(raw[0].__TAURI_INVOKE_KEY__, key)
        window.__TAURI_INTERNALS__.runCallback(raw[0].callback, { result: true })
      }
      assert.equal((await promise).result, true)
      assert.equal(window.__TAURI_INTERNALS__.callbacks.size, 0)
      for (const name of ['invoke', 'ipc', 'postMessage', 'convertFileSrc', 'transformCallback']) {
        assert.equal(window.__TAURI_INTERNALS__[name].toString().includes(key), false)
      }
    }
  })
