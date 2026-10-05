// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT
// Adapted without behavior changes from tauri 2.10.1 scripts/process-ipc-message-fn.js.

(function (message) {
  if (
    message instanceof ArrayBuffer
    || ArrayBuffer.isView(message)
    || Array.isArray(message)
  ) {
    return { contentType: 'application/octet-stream', data: message }
  } else {
    const data = JSON.stringify(message, (_k, val) => {
      const SERIALIZE_TO_IPC_FN = '__TAURI_TO_IPC_KEY__'
      if (val instanceof Map) {
        return Object.fromEntries(val.entries())
      } else if (val instanceof Uint8Array) {
        return Array.from(val)
      } else if (val instanceof ArrayBuffer) {
        return Array.from(new Uint8Array(val))
      } else if (
        typeof val === 'object' && val !== null && SERIALIZE_TO_IPC_FN in val
      ) {
        return val[SERIALIZE_TO_IPC_FN]()
      } else {
        return val
      }
    })
    return { contentType: 'application/json', data }
  }
})
