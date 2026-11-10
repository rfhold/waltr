import { afterEach, describe, expect, test, vi } from "vitest";
import { downloadBlob, fileDeclaration, readTextResult, TextResultBudget, uploadFile } from "../../src/platform/media-transfers";
import { mediaBridge } from "../support/routines";
import type { RoutinePort, RoutineTextResult } from "../../src/generated/waltr-client";

const signal = () => new AbortController().signal;
afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });
const port: RoutinePort = { name: "required/document", direction: "input", media: "file", required: true, maxBytes: "67108864", formats: ["application/pdf"] };
const text: RoutineTextResult = { id: "opaque/text", name: "result", utf8Bytes: "4", sha256: "digest", encodedCharge: "4", helperId: null };
describe("finite media transfers", () => {
  test("declares required PDF/PNG metadata before reading; upload slices sequential 64 KiB and commits the exact port", async () => {
    const bridge = mediaBridge();
    const file = new File([new Uint8Array(131073)], "source.pdf", { type: "application/pdf" });
    const whole = vi.spyOn(file, "arrayBuffer");
    expect(fileDeclaration(port, file)).toEqual({ format: "file", port: port.name, displayName: "source.pdf", contentType: "application/pdf", declaredBytes: 131073, expectedSha256: null });
    const image = new File([new Uint8Array(8)], "source.png", { type: "image/png" });
    expect(fileDeclaration({ ...port, media: "image", formats: ["png"] }, image)).toMatchObject({ format: "image", image: "png", declaredBytes: 8 });
    let release!: () => void;
    bridge.appendMediaUpload.mockImplementationOnce(() => new Promise<void>(resolve => { release = resolve; }));
    const upload = uploadFile(bridge, 7, port.name, file, signal());
    await vi.waitFor(() => expect(bridge.appendMediaUpload).toHaveBeenCalledOnce());
    expect(bridge.commitMediaUpload).not.toHaveBeenCalled(); release(); await upload;
    expect(whole).not.toHaveBeenCalled();
    expect(bridge.beginMediaUpload).toHaveBeenCalledExactlyOnceWith(7, port.name);
    expect(bridge.appendMediaUpload.mock.calls.map(call => call[2].length)).toEqual([65536, 65536, 1]);
    expect(bridge.commitMediaUpload).toHaveBeenCalledExactlyOnceWith(7, port.name);
  });
  test("rejects oversized images and stops upload after ambiguous append without retry/commit", async () => {
    expect(() => fileDeclaration({ ...port, media: "image" }, { size: 20971521, type: "image/png" } as File)).toThrow("limit");
    const bridge = mediaBridge(); bridge.appendMediaUpload.mockRejectedValueOnce(new Error("lost ack"));
    await expect(uploadFile(bridge, 7, port.name, new File([new Uint8Array(70000)], "a.pdf"), signal())).rejects.toThrow("lost ack");
    expect(bridge.appendMediaUpload).toHaveBeenCalledOnce(); expect(bridge.commitMediaUpload).not.toHaveBeenCalled();
  });
  test("bounded explicit Blob save closes its owned reader; expiry closes with no retry", async () => {
    const bridge = mediaBridge();
    bridge.beginMediaDownload.mockResolvedValue(JSON.stringify({ bytes: "3", displayName: "out.pdf", contentType: "application/pdf", remainingValidityMs: "900000" }));
    bridge.readMediaDownloadChunk.mockResolvedValueOnce(new Uint8Array([1, 2, 3])).mockResolvedValueOnce(undefined);
    const result = await downloadBlob(bridge, 7, "output", signal()); expect(result.blob.size).toBe(3); expect(result.name).toBe("out.pdf");
    expect(bridge.closeMediaDownload).toHaveBeenCalledExactlyOnceWith(7, "output");
    bridge.beginMediaDownload.mockResolvedValue(JSON.stringify({ bytes: "1", remainingValidityMs: "0" }));
    await expect(downloadBlob(bridge, 7, "expired", signal())).rejects.toThrow("expired");
    expect(bridge.readMediaDownloadChunk).toHaveBeenCalledTimes(2);
    expect(bridge.closeMediaDownload).toHaveBeenLastCalledWith(7, "expired");
  });
  test("rejects oversized Blob fallback before read and closes on truncated data", async () => {
    const bridge = mediaBridge(); bridge.beginMediaDownload.mockResolvedValue('{"bytes":"67108865"}');
    await expect(downloadBlob(bridge, 7, "out", signal())).rejects.toThrow("64 MiB");
    expect(bridge.readMediaDownloadChunk).not.toHaveBeenCalled(); expect(bridge.closeMediaDownload).toHaveBeenCalledOnce();
    bridge.beginMediaDownload.mockResolvedValue('{"bytes":"10","remainingValidityMs":"900000"}');
    await expect(downloadBlob(bridge, 7, "out", signal())).rejects.toThrow("Incomplete");
  });
  test("a complete download cannot succeed when owned reader closure rejects", async () => {
    const bridge = mediaBridge();
    bridge.beginMediaDownload.mockResolvedValue('{"bytes":"1","remainingValidityMs":"900000"}');
    bridge.readMediaDownloadChunk.mockResolvedValueOnce(new Uint8Array([1])).mockResolvedValueOnce(undefined);
    bridge.closeMediaDownload.mockRejectedValue(new Error("private cleanup failure"));
    await expect(downloadBlob(bridge, 7, "out", signal())).rejects.toThrow("cleanup could not be verified");
    expect(bridge.closeMediaDownload).toHaveBeenCalledOnce();
  });
  test.each(["abort", "expiry"])("%s actively closes and wakes a blocked owned read, then joins closure", async reason => {
    vi.useFakeTimers();
    const bridge = mediaBridge(), controller = new AbortController();
    let wake!: (value: undefined) => void, finishClose!: () => void;
    bridge.beginMediaDownload.mockResolvedValue('{"bytes":"1","remainingValidityMs":"1"}');
    bridge.readMediaDownloadChunk.mockImplementation(() => new Promise(resolve => { wake = resolve; }));
    bridge.closeMediaDownload.mockImplementation(() => { wake(undefined); return new Promise<void>(resolve => { finishClose = resolve; }); });
    let settled = false; const result = downloadBlob(bridge, 7, "out", controller.signal).catch(error => { settled = true; return error; });
    await vi.advanceTimersByTimeAsync(0);
    expect(bridge.readMediaDownloadChunk).toHaveBeenCalledOnce();
    if (reason === "abort") controller.abort(); else await vi.advanceTimersByTimeAsync(2);
    await vi.advanceTimersByTimeAsync(0);
    expect(bridge.closeMediaDownload).toHaveBeenCalledExactlyOnceWith(7, "out"); expect(settled).toBe(false);
    finishClose(); expect(await result).toBeInstanceOf(Error);
  });
  test("abort reaches close while checkout itself is pending, without abandoning that checkout", async () => {
    const bridge = mediaBridge(), controller = new AbortController();
    let reject!: (error: Error) => void, finishClose!: () => void;
    bridge.beginMediaDownload.mockImplementation(() => new Promise((_resolve, no) => { reject = no; }));
    bridge.closeMediaDownload.mockImplementation(() => { reject(new Error("checkout cancelled")); return new Promise<void>(resolve => { finishClose = resolve; }); });
    let settled = false; const result = downloadBlob(bridge, 7, "out", controller.signal).catch(error => { settled = true; return error; });
    controller.abort(); await vi.waitFor(() => expect(bridge.closeMediaDownload).toHaveBeenCalledOnce());
    expect(settled).toBe(false); finishClose(); expect(await result).toBeInstanceOf(Error);
  });
  test.each([undefined, null, "-1", "NaN", "Infinity", "900001"])("invalid lifetime %s never starts a new fifteen-minute grant", async ttl => {
    const bridge = mediaBridge(); bridge.beginMediaDownload.mockResolvedValue(JSON.stringify({ bytes: "1", remainingValidityMs: ttl }));
    await expect(downloadBlob(bridge, 7, "out", signal())).rejects.toThrow("lifetime");
    expect(bridge.readMediaDownloadChunk).not.toHaveBeenCalled(); expect(bridge.closeMediaDownload).toHaveBeenCalledOnce();
  });
  test("checkout time is charged against the original monotonic grant deadline", async () => {
    const bridge = mediaBridge();
    vi.spyOn(performance, "now").mockReturnValueOnce(100).mockReturnValue(102);
    bridge.beginMediaDownload.mockResolvedValue('{"bytes":"1","remainingValidityMs":"1"}');
    await expect(downloadBlob(bridge, 7, "out", signal())).rejects.toThrow("expired");
    expect(bridge.readMediaDownloadChunk).not.toHaveBeenCalled(); expect(bridge.closeMediaDownload).toHaveBeenCalledOnce();
  });
  test("expiry during final reader closure still prevents successful Blob publication", async () => {
    const bridge = mediaBridge(); let now = 100, close!: () => void;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    bridge.beginMediaDownload.mockResolvedValue('{"bytes":"1","remainingValidityMs":"10"}');
    bridge.readMediaDownloadChunk.mockResolvedValueOnce(new Uint8Array([1])).mockResolvedValueOnce(undefined);
    bridge.closeMediaDownload.mockImplementation(() => new Promise<void>(resolve => { close = resolve; }));
    const result = downloadBlob(bridge, 7, "out", signal());
    await vi.waitFor(() => expect(bridge.closeMediaDownload).toHaveBeenCalledOnce());
    now = 111; close(); await expect(result).rejects.toThrow("expired");
  });
  test("durable decoded UTF-8 is independent of binary grants and reserves aggregate text budget", async () => {
    const bridge = mediaBridge(); bridge.readRoutineTextResultChunk.mockResolvedValueOnce("😀").mockResolvedValueOnce(undefined);
    expect(await readTextResult(bridge, "historical", text, signal())).toBe("😀");
    expect(bridge.beginMediaDownload).not.toHaveBeenCalled(); expect(bridge.closeRoutineTextResult).toHaveBeenCalledExactlyOnceWith(9);
    const budget = new TextResultBudget(); const releases = Array.from({ length: 8 }, () => budget.reserve({ ...text, utf8Bytes: "1048576" }));
    expect(() => budget.reserve(text)).toThrow("8 MiB"); releases[0](); releases[0](); expect(() => budget.reserve(text)).not.toThrow();
  });
});
