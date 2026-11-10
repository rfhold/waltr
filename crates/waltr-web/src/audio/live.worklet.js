class LivePcmCapture extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.rate = options.processorOptions.sampleRate;
    this.buffer = new Int16Array(32768);
    this.used = 0;
    this.credit = true;
    this.stopping = false;
    this.ended = false;
    this.frame = 0;
    this.output = 0;
    this.previous = 0;
    this.port.onmessage = ({ data }) => {
      if (this.ended) return;
      if (data.type === "ack") { this.credit = true; this.flush(); }
      if (data.type === "stop") {
        this.stopping = true;
        while (this.output * sampleRate / this.rate < this.frame) this.append(this.previous);
        this.flush();
      }
    };
  }
  fail() { this.ended = true; this.port.postMessage({ type: "error" }); }
  append(value) {
    if (this.used === this.buffer.length) { this.fail(); return; }
    value = Math.max(-1, Math.min(1, value));
    this.buffer[this.used++] = Math.round(value * (value < 0 ? 32768 : 32767));
    this.output++;
  }
  flush() {
    if (this.ended || !this.credit) return;
    if (this.used && (this.used >= 2048 || this.stopping)) {
      const bytes = new Uint8Array(this.used * 2);
      const view = new DataView(bytes.buffer);
      for (let i = 0; i < this.used; i++) view.setInt16(i * 2, this.buffer[i], true);
      this.used = 0;
      this.credit = false;
      this.port.postMessage({ type: "pcm", bytes }, [bytes.buffer]);
    } else if (this.stopping && !this.used) {
      this.ended = true;
      this.port.postMessage({ type: "stopped" });
    }
  }
  process(inputs) {
    if (this.ended) return false;
    if (this.stopping) return true;
    const channels = inputs[0];
    if (!channels?.length) return true;
    if (channels.length > 2) { this.fail(); return false; }
    for (let i = 0; i < channels[0].length; i++) {
      let value = 0;
      for (const channel of channels) value += channel[i] / channels.length;
      if (!Number.isFinite(value)) { this.fail(); return false; }
      const index = this.frame++;
      while (!this.ended && this.output * sampleRate / this.rate <= index) {
        const fraction = this.output * sampleRate / this.rate - (index - 1);
        this.append(index === 0 ? value : this.previous + (value - this.previous) * fraction);
      }
      this.previous = value;
    }
    this.flush();
    return !this.ended;
  }
}
registerProcessor("waltr-live-pcm", LivePcmCapture);
