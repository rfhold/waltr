// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT
// Adapted from tauri 2.10.1 scripts/ipc-protocol.js. Keep transport behavior in sync.

;(function () {
  const osName = __WALTR_OS_NAME__
  const location = window.location
  const packaged = osName === 'windows'
    ? location.protocol === 'http:' && location.host === 'tauri.localhost'
    : (osName === 'macos' || osName === 'linux')
      && location.protocol === 'tauri:' && location.host === 'localhost'

  // This check runs before creating any key-bearing closure, including in Wry subframes.
  if (window.top !== window || !packaged) return

  // Only this private lexical scope contains the substituted key literal.
  const __TAURI_INVOKE_KEY__ = __INVOKE_KEY__
  const processIpcMessage = __WALTR_PROCESS_IPC_MESSAGE_FN__
  const fetchChannelDataCommand = 'plugin:__TAURI_CHANNEL__|fetch'
  let customProtocolIpcFailed = false
  const canUseCustomProtocol = osName !== 'android'

  function sendIpcMessage(message) {
    const { cmd, callback, error, payload, options } = message
    if (
      !customProtocolIpcFailed
      && (canUseCustomProtocol || cmd === fetchChannelDataCommand)
    ) {
      const { contentType, data } = processIpcMessage(payload)
      const headers = new Headers((options && options.headers) || {})
      headers.set('Content-Type', contentType)
      headers.set('Tauri-Callback', callback)
      headers.set('Tauri-Error', error)
      headers.set('Tauri-Invoke-Key', __TAURI_INVOKE_KEY__)
      fetch(window.__TAURI_INTERNALS__.convertFileSrc(cmd, 'ipc'), {
        method: 'POST', body: data, headers
      })
        .then((response) => {
          const callbackId = response.headers.get('Tauri-Response') === 'ok' ? callback : error
          switch ((response.headers.get('content-type') || '').split(',')[0]) {
            case 'application/json':
              return response.json().then((r) => [callbackId, r])
            case 'text/plain':
              return response.text().then((r) => [callbackId, r])
            default:
              return response.arrayBuffer().then((r) => [callbackId, r])
          }
        })
        .then(
          ([callbackId, data]) => window.__TAURI_INTERNALS__.runCallback(callbackId, data),
          (e) => {
            console.warn('IPC custom protocol failed, Tauri will now use the postMessage interface instead', e)
            customProtocolIpcFailed = true
            sendIpcMessage(message)
          }
        )
    } else {
      const { data } = processIpcMessage({
        cmd, callback, error,
        options: { ...options, customProtocolIpcBlocked: customProtocolIpcFailed },
        payload, __TAURI_INVOKE_KEY__
      })
      window.ipc.postMessage(data)
    }
  }

  Object.defineProperty(window.__TAURI_INTERNALS__, 'postMessage', {
    value: sendIpcMessage
  })
})()
