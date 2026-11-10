type Format = { type: "pcm16le"; sampleRate: number; channels: number };
type Stream = { format: Format; active: boolean; closed: boolean; pending: number; end: number };
/** At most 256 KiB / 32 scheduled chunks. A stalled sink cancels, never grows a recording. */
export class PcmPlayback {
  private context: AudioContext;
  private nodes = new Set<AudioBufferSourceNode>();
  private bytes = 0;
  private nextTime = 0;
  private disposed = false;
  private disposal: Promise<void> | undefined;
  private failure: Error | undefined;
  private streams = new Map<string, Stream>();
  constructor(private readonly onFailure: (error: Error) => void = () => {}) { this.context = new AudioContext(); void this.context.resume().catch(() => this.fail("Playback could not start.")); }
  private fail(message: string) {
    if (this.disposed || this.failure) return;
    this.failure = new Error(message);
    this.onFailure(this.failure);
    void this.dispose().catch(() => undefined);
  }
  check() {
    if (!this.failure && !this.disposed) for (const stream of this.streams.values()) {
      if (stream.active && !stream.closed && this.context.currentTime >= stream.end) { this.fail("Active PCM output underrun."); break; }
    }
    if (this.failure) throw this.failure;
  }
  open(stream: string, format: Format) {
    if (this.disposed || this.streams.has(stream) || format.type !== "pcm16le" || !Number.isInteger(format.sampleRate) || format.sampleRate < 8000 || format.sampleRate > 96000 || ![1, 2].includes(format.channels)) throw new Error("Unsupported playback format.");
    if (this.streams.size >= 32) throw new Error("Playback stream limit.");
    this.check();
    this.streams.set(stream, { format, active: false, closed: false, pending: 0, end: 0 });
  }
  push(stream: string, bytes: Uint8Array) {
    this.check();
    const state = this.streams.get(stream), format = state?.format;
    if (this.disposed || !format || state!.closed || !(bytes instanceof Uint8Array) || !bytes.length || bytes.length > 65536 || bytes.length % (format.channels * 2)) throw new Error("Invalid PCM output.");
    if (this.context.state !== "running" || this.bytes + bytes.length > 262144 || this.nodes.size >= 32) throw new Error("Playback queue is full or suspended; output was cancelled.");
    const frames = bytes.length / (format.channels * 2);
    const buffer = this.context.createBuffer(format.channels, frames, format.sampleRate);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let channel = 0; channel < format.channels; channel++) {
      const output = buffer.getChannelData(channel);
      for (let frame = 0; frame < frames; frame++) output[frame] = view.getInt16((frame * format.channels + channel) * 2, true) / 32768;
    }
    const node = this.context.createBufferSource(); node.buffer = buffer; node.connect(this.context.destination);
    this.bytes += bytes.length; this.nodes.add(node);
    state!.pending++;
    node.onended = () => {
      this.bytes -= bytes.length; this.nodes.delete(node); node.disconnect(); state!.pending--;
      if (!state!.pending) {
        if (state!.closed) this.streams.delete(stream);
        else this.fail("Active PCM output underrun.");
      }
    };
    const when = Math.max(this.context.currentTime, this.nextTime);
    node.start(when); this.nextTime = when + buffer.duration;
    state!.active = true; state!.end = this.nextTime;
  }
  close(stream: string) { this.check(); const state = this.streams.get(stream); if (state) { state.closed = true; if (!state.pending) this.streams.delete(stream); } }
  async finish() {
    this.check();
    if ([...this.streams.values()].some(stream => !stream.closed)) { this.fail("PCM output was not closed."); this.check(); }
    const deadline = Date.now() + 30000;
    while (!this.disposed && this.nodes.size) {
      this.check();
      if (Date.now() >= deadline || this.context.state !== "running") { await this.dispose(); throw new Error("Playback did not drain."); }
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    this.check();
    await this.dispose();
  }
  dispose(): Promise<void> {
    if (this.disposal) return this.disposal;
    this.disposed = true;
    for (const node of this.nodes) { node.onended = null; node.stop(); node.disconnect(); }
    this.nodes.clear(); this.streams.clear(); this.bytes = 0;
    this.disposal = this.context.close();
    return this.disposal;
  }
}
