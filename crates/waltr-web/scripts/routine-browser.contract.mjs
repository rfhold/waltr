import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { createServer } from "vite";
import { chromium } from "playwright";

// This fake speaks the checked-in v2 protobuf over gRPC-web to the real WASM.
// It does not model server admission policy or replace server integration tests.
const source = (await Promise.all(["routine/v2/routine.proto", "status/v1/status_service.proto"].map(path => readFile(new URL(`../../../proto/waltr/${path}`, import.meta.url), "utf8")))).join("\n").replace(/\/\/[^\n]*/g, "");
const messages = new Map();
for (const match of source.matchAll(/message\s+(\w+)\s*\{/g)) {
  let end = match.index + match[0].length, depth = 1;
  for (; depth; end++) { if (source[end] === "{") depth++; if (source[end] === "}") depth--; }
  const fields = [...source.slice(match.index + match[0].length, end - 1).matchAll(/\b(?:(repeated|optional)\s+)?(\w+)\s+(\w+)\s*=\s*(\d+)\s*;/g)].map(([, repeated, type, name, tag]) => ({ repeated: repeated === "repeated", type, name, tag: Number(tag) }));
  messages.set(match[1], fields);
}
function varint(n) { n = BigInt(n); const bytes = []; do { bytes.push(Number(n & 127n) | (n > 127n ? 128 : 0)); n >>= 7n; } while (n); return Buffer.from(bytes); }
function encode(type, value) {
  return Buffer.concat(messages.get(type).flatMap(field => (field.repeated ? value[field.name] ?? [] : value[field.name] === undefined ? [] : [value[field.name]]).map(item => {
    const data = messages.has(field.type) ? encode(field.type, item) : field.type === "string" ? Buffer.from(item) : field.type === "bytes" ? Buffer.from(item) : null;
    return data ? Buffer.concat([varint(field.tag * 8 + 2), varint(data.length), data]) : Buffer.concat([varint(field.tag * 8), varint(item)]);
  })));
}
function decode(type, data) {
  let offset = 0;
  function integer() { let n = 0n, shift = 0n; for (;;) { const b = data[offset++]; n |= BigInt(b & 127) << shift; if (!(b & 128)) return n; shift += 7n; } }
  const result = {};
  while (offset < data.length) {
    const key = Number(integer()), field = messages.get(type).find(f => f.tag === key >> 3);
    let value;
    if ((key & 7) === 2) { const size = Number(integer()), part = data.subarray(offset, offset + size); offset += size; value = !field ? null : messages.has(field.type) ? decode(field.type, part) : field.type === "string" ? part.toString() : part; }
    else value = integer();
    if (field) { if (field.repeated) (result[field.name] ??= []).push(value); else result[field.name] = value; }
  }
  return result;
}
function frame(type, value) { const body = encode(type, value), head = Buffer.alloc(5); head.writeUInt32BE(body.length, 1); return Buffer.concat([head, body]); }
const trailer = Buffer.concat([Buffer.from([128, 0, 0, 0, 16]), Buffer.from("grpc-status: 0\r\n")]);
const limits = Object.fromEntries(messages.get("ServerLimits").map(f => [f.name, 32]));
Object.assign(limits, { chunk_bytes: 65536, input_batch_bytes: 131072, input_batch_events: 32, edge_bytes: 131072, edge_chunks: 32, input_bytes: 268435456, output_bytes: 268435456, input_duration_ms: 3900000, total_duration_ms: 5400000, queue_wait_ms: 30000, idle_ms: 30000, streams_per_direction: 32, text_segments: 64, retained_text_bytes: 1048576, attachment_lease_ms: 30000, attachment_attach_timeout_ms: 10000, attachment_absolute_lifetime_ms: 5400000, file_bytes: 67108864, image_bytes: 20971520, image_dimension: 8192, image_pixels: 32000000, helper_attachment_bytes: 8388608, helper_source_bytes: 16777216, helper_encoded_bytes: 33554432, helper_attachments: 8, binary_output_lease_ms: 900000, text_result_bytes: 1048576, text_results_encoded_bytes: 8388608, text_results: 32, page_size: 100, structured_value_bytes: 65536, definition_bytes: 262144, definitions_per_snapshot: 128, definition_snapshot_bytes: 8388608, operation_projection_bytes: 4096, blob_invocation_bytes: 268435456, blob_principal_bytes: 536870912, blob_objects: 64, blob_readers: 4, ordinary_deadline_ms: 30000 });
const contract = { schema_version: "waltr.value/v1", schema_json: Buffer.from('{"type":"object","properties":{},"required":[]}') };
const definition = { name: "routine", description: "Browser wire proof", revision: "revision", api_version: "waltr.dev/v1alpha1", engine: "javascript", entrypoints: [{ name: "live", trigger: 3, start_contexts: [1, 2] }], parameters: contract, result: contract, ports: [{ name: "audio", direction: 1, media: 1, required: true, max_bytes: 268435456, formats: ["pcm16le"] }, { name: "text", direction: 2, media: 2, required: true, max_bytes: 1048576, formats: ["text"] }], budgets: { value_bytes: 65536, result_bytes: 65536, host_calls: 128, in_flight: 8, deadline_ms: 5400000 } };
definition.ports.push({ name: "speech", direction: 2, media: 1, required: false, max_bytes: 1048576, formats: ["pcm16le"] });
const binaryDefinition = { ...definition, ports: [
  { name: "document", direction: 1, media: 4, required: true, max_bytes: 67108864, formats: ["application/pdf"] },
  { name: "image", direction: 1, media: 3, required: true, max_bytes: 20971520, formats: ["image/png"] },
  { name: "text", direction: 2, media: 2, required: true, max_bytes: 1048576, formats: ["text"] },
  { name: "result", direction: 2, media: 4, required: true, max_bytes: 67108864, formats: ["application/pdf"] },
] };
const outputBytes = Buffer.from("bounded output file");
const outputBlob = { blob_id: "public-output", metadata: { display_name: "result.pdf", content_type: "application/pdf", bytes: outputBytes.length, sha256: createHash("sha256").update(outputBytes).digest() }, remaining_validity_ms: 900000 };
const accepted = { invocation_id: "9b5080e5-2f32-442f-acbf-53e6d997c911", revision: "revision", limits };
const credentials = { attachment_id: "attachment", secret: Buffer.alloc(32, 71), invocation_id: accepted.invocation_id, generation: "generation" };
let output, pcmChunks = 0, maxChunk = 0, finished = false, admission = 0;
let mode = "success", renewed = 0, cancelled = 0, held, absoluteExpires;
let declarations, uploadChunks = [], committedPorts = [], blobInputs = [], readerClosed = 0;
let openAborted = 0, readAborted = 0, fileGrantExpires, readStarted, admissionStarted, queuedReply = false;
const filesMode = () => mode.startsWith("files");
const fileBlob = () => ({ ...outputBlob, remaining_validity_ms: fileGrantExpires ? Math.max(0, fileGrantExpires - Date.now()) : 900000 });
const uploads = new Map();
function terminal() {
  finished = true;
  if (filesMode()) {
    assert.deepEqual(blobInputs.sort(), ["document", "image"]);
    if (["files-expiry", "files-queued-expiry"].includes(mode)) fileGrantExpires = Date.now() + 1000;
    output.write(frame("WatchLiveOutputResponse", { output: { blob: { port: "result", blob: fileBlob(), output_grant: { invocation_id: accepted.invocation_id, port: "result", blob_id: "public-output", generation: "generation", secret: Buffer.from("SECRET_GRANT_CANARY_01234567890123") } } } }));
  }
  output.write(frame("WatchLiveOutputResponse", { output: { chunk: { stream_id: "transcript", sequence: filesMode() ? 0 : 1, text: { segment_id: "segment", mutation: 2, text: "Final transcript", committed: true } } } }));
  output.write(frame("WatchLiveOutputResponse", { output: { close: { stream_id: "transcript", next_sequence: filesMode() ? 1 : 2 } } }));
  if (mode !== "missing-terminal") output.write(frame("WatchLiveOutputResponse", { output: { terminal: { status: 3, result: { json: Buffer.from("{}") }, cleanup_complete: true } } }));
  output.end(trailer);
}
const middleware = async (req, res, next) => {
  if (!req.url?.includes("/waltr.")) return next();
  const method = req.url.split("/").at(-1);
  const parts = []; for await (const part of req) parts.push(part);
  const body = Buffer.concat(parts);
  res.setHeader("content-type", "application/grpc-web+proto");
  res.setHeader("access-control-expose-headers", "grpc-status,grpc-message");
  function reply(type, value) { res.end(Buffer.concat([frame(type, value), trailer])); }
  if (method === "WatchStatus") { res.write(frame("WatchStatusResponse", { status: { ready: true } })); return; }
  if (method === "WatchDefinitions") { res.write(frame("WatchDefinitionsResponse", { catalog_revision: "catalog", definitions: [filesMode() ? binaryDefinition : definition], limits })); return; }
  if (method === "OpenAttachment") {
    const value = decode("OpenAttachmentRequest", body.subarray(5));
    if (filesMode()) {
      declarations = new Map(value.open.inputs.map(input => [input.port, input.binary]));
      assert.deepEqual([...declarations.keys()].sort(), ["document", "image"]);
      assert.equal(declarations.get("document").declared_bytes, 131073n); assert.equal(declarations.get("image").image, 1n);
      assert.equal(uploadChunks.length, 0, "required metadata precedes upload");
    } else assert.equal(value.open.inputs[0].port, "audio");
    assert.equal(value.open.selection.revision, "revision"); admission++;
    admissionStarted?.();
    if (mode.startsWith("pending-open")) { res.once("close", () => { openAborted++; }); return; }
    absoluteExpires = Date.now() + (mode === "expiry" ? 1500 : 5400000);
    return reply("OpenAttachmentResponse", { accepted: { invocation: accepted, credentials, lease_expires_ms: Math.min(Date.now() + 30000, absoluteExpires), absolute_expires_ms: absoluteExpires, attach_by_ms: Math.min(Date.now() + 10000, absoluteExpires) } });
  }
  if (method === "WatchLiveOutput") {
    output = res; output.write(frame("WatchLiveOutputResponse", { output: { accepted } }));
    if (!filesMode()) {
      output.write(frame("WatchLiveOutputResponse", { output: { open: { port: "speech", stream_id: "playback", pcm16le: { sample_rate: 16000, channels: 2 } } } }));
      output.write(frame("WatchLiveOutputResponse", { output: { chunk: { stream_id: "playback", sequence: 0, audio: Buffer.alloc(mode === "end-tail" ? 65536 : mode === "active-underrun" ? 32768 : 4096) } } }));
      if (mode !== "active-underrun") output.write(frame("WatchLiveOutputResponse", { output: { close: { stream_id: "playback", next_sequence: 1 } } }));
    }
    output.write(frame("WatchLiveOutputResponse", { output: { open: { port: "text", stream_id: "transcript", text: {} } } })); return;
  }
  if (method === "SendInputBatch") {
    const value = decode("SendInputBatchRequest", body.subarray(5));
    for (const event of value.events) {
      if (event.finish) terminal();
      if (event.blob) { assert.ok(committedPorts.includes(event.blob.port)); assert.equal(event.blob.blob_id, `blob-${event.blob.port}`); blobInputs.push(event.blob.port); }
      if (event.chunk?.audio) {
        pcmChunks++; maxChunk = Math.max(maxChunk, event.chunk.audio.length);
        if (pcmChunks === 1) output.write(frame("WatchLiveOutputResponse", { output: { chunk: { stream_id: "transcript", sequence: 0, text: { segment_id: "segment", mutation: 2, text: "LIVE before release", committed: false } } } }));
        if (mode === "saturated-cancel") { held = res; return; }
        if (mode === "lost-ack") { res.setHeader("grpc-status", "14"); res.end(); return; }
      }
    }
    return reply("SendInputBatchResponse", { accepted_sequence: value.sequence ?? 0 });
  }
  if (method === "CloseAttachment") {
    const value = decode("CloseAttachmentRequest", body.subarray(5));
    if (value.mode === 1n) {
      terminal();
    } else { cancelled++; held?.end(); output?.end(); }
    return reply("CloseAttachmentResponse", { status: 3, cleanup_complete: true });
  }
  if (method === "RenewAttachment") { renewed++; return reply("RenewAttachmentResponse", { lease_expires_ms: Math.min(Date.now() + 30000, absoluteExpires), absolute_expires_ms: absoluteExpires }); }
  if (method === "BeginBlobUpload") {
    const value = decode("BeginBlobUploadRequest", body.subarray(5)), pinned = declarations.get(value.port);
    assert.equal(value.declared_bytes, pinned.declared_bytes); assert.equal(value.display_name, pinned.display_name); assert.equal(value.content_type, pinned.content_type); assert.equal(value.image, pinned.image);
    uploads.set(value.port, { value, parts: [], offset: 0, sequence: 0 });
    return reply("BeginBlobUploadResponse", { upload_id: value.port, chunk_bytes: 65536 });
  }
  if (method === "AppendBlobUpload") {
    const value = decode("AppendBlobUploadRequest", body.subarray(5)), upload = uploads.get(value.upload_id);
    assert.equal(Number(value.offset ?? 0), upload.offset); assert.equal(Number(value.sequence ?? 0), upload.sequence++); assert.ok(value.data.length <= 65536);
    upload.parts.push(value.data); upload.offset += value.data.length; uploadChunks.push(value.data.length);
    return reply("AppendBlobUploadResponse", { accepted_sequence: value.sequence ?? 0, next_offset: upload.offset });
  }
  if (method === "CommitBlobUpload") {
    const value = decode("CommitBlobUploadRequest", body.subarray(5)), upload = uploads.get(value.upload_id);
    assert.equal(Number(value.bytes), upload.offset); assert.equal(Number(value.next_sequence), upload.sequence);
    committedPorts.push(upload.value.port);
    return reply("CommitBlobUploadResponse", { blob: { blob_id: `blob-${upload.value.port}`, metadata: { display_name: upload.value.display_name, content_type: upload.value.content_type, bytes: upload.offset, sha256: createHash("sha256").update(Buffer.concat(upload.parts)).digest(), ...(upload.value.image ? { image: { format: 1, width: 1, height: 1 } } : {}) } } });
  }
  if (method === "OpenBlobRead") {
    const value = decode("OpenBlobReadRequest", body.subarray(5)); assert.equal(value.output_grant.blob_id, "public-output");
    return reply("OpenBlobReadResponse", { credentials: { invocation_id: accepted.invocation_id, reader_id: "private-reader", generation: "generation", secret: Buffer.alloc(32, 76) }, blob: fileBlob() });
  }
  if (method === "ReadBlob") {
    const value = decode("ReadBlobRequest", body.subarray(5)); assert.equal(value.max_bytes, 65536n);
    readStarted?.();
    if (["files-abort", "files-expiry"].includes(mode)) { res.flushHeaders(); res.once("close", () => { readAborted++; }); return; }
    if (mode === "files-queued-expiry") { setTimeout(() => { queuedReply = true; reply("ReadBlobResponse", { data: outputBytes, sequence: 0, eof: true }); }, 100); return; }
    res.write(frame("ReadBlobResponse", { data: outputBytes, sequence: 0, eof: true })); res.end(trailer); return;
  }
  if (method === "CloseBlobRead") { readerClosed++; if (mode === "files-close-reject") { res.setHeader("grpc-status", "14"); res.end(); return; } return reply("CloseBlobReadResponse", {}); }
  res.setHeader("grpc-status", "12"); res.end();
};
const server = await createServer({ server: { host: "127.0.0.1", port: 0 }, configFile: false, optimizeDeps: { noDiscovery: true }, plugins: [{ name: "routine-wire-fixture", configureServer(server) { server.middlewares.use(middleware); } }] });
let browser;
try {
  await server.listen();
  browser = await chromium.launch({ headless: true, args: ["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream", "--autoplay-policy=no-user-gesture-required"] });
  for (const scenario of ["success", "saturated-cancel", "lost-ack", "missing-terminal", "expiry", "active-underrun", "end-tail"]) {
  mode = scenario; output = undefined; held = undefined; pcmChunks = 0; maxChunk = 0; finished = false; admission = 0; renewed = 0; cancelled = 0;
  const page = await browser.newPage();
  page.on("pageerror", error => console.error("browser:", error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/scripts/routine-browser.html`);
  try { await page.waitForFunction(() => window.routineProof?.ready(), undefined, { timeout: 15000 }); }
  catch (error) { console.log(await page.evaluate(() => window.routineProof?.snapshot())); throw error; }
  await page.click("#start");
  if (scenario !== "lost-ack") await page.waitForFunction(() => window.routineProof.state().liveBeforeRelease, undefined, { timeout: 15000 });
  else await page.waitForFunction(() => window.routineProof.state().status?.phase === "error", undefined, { timeout: 15000 });
  assert.equal(finished, false); assert.ok(pcmChunks > 0); assert.ok(maxChunk <= 65536);
  if (scenario === "success") {
    await page.waitForTimeout(5200); assert.ok(renewed > 0, "lease renewal runs during capture");
  }
   const finishAt = Date.now();
   if (scenario === "saturated-cancel") await page.click("#cancel");
    else if (!["lost-ack", "expiry", "active-underrun"].includes(scenario)) await page.click("#finish");
   if (scenario === "end-tail") {
     await page.waitForTimeout(100);
     assert.notEqual((await page.evaluate(() => window.routineProof.state())).status.phase, "succeeded", "explicit close retains the scheduled audio tail");
   }
   const expected = ["success", "end-tail"].includes(scenario) ? "succeeded" : scenario === "saturated-cancel" ? "cancelled" : "error";
  try { await page.waitForFunction(expected => window.routineProof.state().status?.phase === expected, expected, { timeout: 15000 }); }
  catch (error) { console.log(await page.evaluate(() => ({ ...window.routineProof.state(), media: window.routineProof.mediaStatus() }))); throw error; }
  const proof = await page.evaluate(() => { const state = window.routineProof.state(); window.routineProof.dispose(); return state; });
   if (scenario === "success" || scenario === "end-tail") assert.equal(proof.status.finalText, "Final transcript");
  else { await page.waitForTimeout(100); assert.ok(cancelled > 0, "cancellation bypasses producer credit"); }
  assert.equal(admission, 1);
   if (scenario === "lost-ack") assert.equal(pcmChunks, 1, "ambiguous PCM acknowledgment never retries");
   if (scenario === "active-underrun") { assert.equal(finished, false); assert.equal(proof.released, false); }
   if (scenario === "end-tail") assert.ok(Date.now() - finishAt >= 500, "success waits for the actual scheduled last node");
  console.log(JSON.stringify({ scenario, browser: browser.version(), actualWasm: true, fakeV2GrpcWeb: true, actualAudioWorklet: true, actualPcmPlayback: true, pcmChunks, maxChunk, renewed, cancelled, ...proof }));
  await page.close();
  }
  for (const scenario of ["pending-open-cancel", "pending-open-timeout"]) {
    mode = scenario; admission = 0; openAborted = 0;
    const started = new Promise(resolve => { admissionStarted = resolve; });
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/scripts/routine-browser.html`);
    await page.waitForFunction(() => window.routineProof?.ready());
    const began = Date.now();
    if (scenario === "pending-open-cancel") await page.click("#start");
    else assert.equal(await page.evaluate(() => window.routineProof.openOnly()), 1);
    await started;
    assert.equal(JSON.parse(await page.evaluate(() => window.routineProof.mediaStatus())).phase, "opening");
    if (scenario === "pending-open-cancel") {
      await page.click("#cancel");
      await page.waitForFunction(() => window.routineProof.state().status?.phase === "error");
      assert.equal(await page.evaluate(() => window.routineProof.tracksEnded()), true);
    } else {
      await page.waitForFunction(() => JSON.parse(window.routineProof.mediaStatus()).phase === "error", undefined, { timeout: 25000 });
      assert.ok(Date.now() - began >= 19500, "the owned Open-to-Accepted deadline remains twenty seconds");
    }
    await page.waitForTimeout(100);
    assert.equal(openAborted, 1, "the actual unanswered fetch is terminated");
    assert.equal(admission, 1, "unknown admission is never retried automatically");
    const terminal = JSON.parse(await page.evaluate(() => window.routineProof.mediaStatus()));
    assert.equal(terminal.cleanupVerified, false, "unknown server effects are not verified cleanup");
    assert.ok(Date.now() - began < (scenario === "pending-open-cancel" ? 5000 : 23500));
    await page.click("#start");
    await page.waitForTimeout(200);
    assert.equal(admission, 2, "only an explicit new gesture starts a new owner");
    await page.click("#cancel");
    console.log(JSON.stringify({ scenario, actualWasm: true, fakeV2GrpcWeb: true, elapsedMs: Date.now() - began, firstFetchAborted: true, cleanupVerified: terminal.cleanupVerified, admissionAfterExplicitGesture: admission }));
    admissionStarted = undefined;
    await page.evaluate(() => window.routineProof.dispose()); await page.close();
  }
  for (const scenario of ["files", "files-abort", "files-expiry", "files-close-reject", "files-queued-expiry"]) {
  mode = scenario; admission = 0; uploadChunks = []; committedPorts = []; blobInputs = []; uploads.clear(); readerClosed = 0; readAborted = 0; fileGrantExpires = undefined; queuedReply = false;
  const reading = new Promise(resolve => { readStarted = resolve; });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/scripts/routine-browser.html`);
  await page.waitForFunction(() => window.routineProof?.ready(), undefined, { timeout: 15000 });
  await page.setInputFiles("#document", { name: "source.pdf", mimeType: "application/pdf", buffer: Buffer.alloc(131073, 37) });
  await page.setInputFiles("#image", { name: "source.png", mimeType: "image/png", buffer: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==", "base64") });
  await page.click("#files");
  await reading;
  if (scenario === "files-abort") await page.evaluate(() => window.routineProof.cancelFiles());
  if (scenario === "files-queued-expiry") await page.evaluate(() => { const until = performance.now() + 1500; while (performance.now() < until) {} });
  await page.waitForFunction(() => ["succeeded", "error"].includes(window.routineProof.fileResult()?.phase), undefined, { timeout: 15000 });
  const files = await page.evaluate(() => window.routineProof.fileResult());
  assert.equal(files.phase, scenario === "files" ? "succeeded" : "error");
  if (scenario === "files") assert.equal(files.contents, outputBytes.toString());
  assert.equal(await page.evaluate(() => window.routineProof.fileUrls()), scenario === "files" ? 1 : 0, "unverified/expired output never publishes a Blob URL");
  assert.equal(readerClosed, 1); assert.equal(admission, 1);
  if (["files-abort", "files-expiry"].includes(scenario)) assert.equal(readAborted, 1, "close interrupts the actual pending ReadBlob fetch");
  if (scenario === "files-queued-expiry") assert.equal(queuedReply, true);
  assert.deepEqual(uploadChunks.slice(0, 3), [65536, 65536, 1]); assert.deepEqual(committedPorts.sort(), ["document", "image"]);
  assert.ok(!JSON.stringify(files).includes("SECRET_GRANT_CANARY")); assert.ok(!JSON.stringify(files).includes("private-reader"));
  console.log(JSON.stringify({ scenario, actualWasm: true, fakeV2GrpcWeb: true, requiredMetadataBeforeUpload: true, uploadChunks, committedPorts, typedDownloadBytes: files.size ?? 0, privateReaderClosed: readerClosed, readFetchAborted: readAborted, queuedReplyRejectedAfterExpiry: queuedReply, blobUrls: await page.evaluate(() => window.routineProof.fileUrls()) }));
  await page.evaluate(() => window.routineProof.dispose()); await page.close();
  readStarted = undefined;
  }
} finally { await browser?.close(); await server.close(); }
