/* Standalone static asset: deliberately no imports or browser-main-thread globals. */
class PcmCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.credits = 8;
    this.frames = 0;
    this.channels = 0;
    this.total = 0;
    this.limit = Math.floor(sampleRate * options.processorOptions.durationSeconds);
    this.buffer = new Float32Array(2048 * 2);
    this.ended = false;
    this.port.onmessage = ({ data }) => {
      if (this.ended) return;
      if (data.type === "ack") this.credits = Math.min(8, this.credits + 1);
      if (data.type === "stop") {
        if (this.flush()) this.port.postMessage({ type: "stopped" });
        this.ended = true;
      }
    };
  }

  fail(code) {
    if (!this.ended) this.port.postMessage({ type: "error", code });
    this.ended = true;
    return false;
  }

  flush() {
    if (!this.frames) return true;
    if (!this.credits) return this.fail("overflow");
    const samples = this.buffer.slice(0, this.frames * this.channels);
    this.credits--;
    this.port.postMessage({ type: "samples", channels: this.channels, samples }, [samples.buffer]);
    this.frames = 0;
    return true;
  }

  process(inputs) {
    if (this.ended) return false;
    const channels = inputs[0];
    if (!channels?.length) return true;
    if (channels.length > 2) return this.fail("device");
    if (this.channels && this.channels !== channels.length && !this.flush()) return false;
    this.channels = channels.length;
    const count = channels[0].length;
    if (this.total + count > this.limit) return this.fail("duration");
    this.total += count;
    for (let frame = 0; frame < count; frame++) {
      for (let channel = 0; channel < this.channels; channel++) {
        this.buffer[this.frames * this.channels + channel] = channels[channel][frame];
      }
      this.frames++;
      if (this.frames === 2048 && !this.flush()) return false;
    }
    return true;
  }
}
registerProcessor("waltr-pcm-capture", PcmCaptureProcessor);
