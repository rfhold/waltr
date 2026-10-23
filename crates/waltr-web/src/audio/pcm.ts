export const MAX_DURATION_SECONDS = 300;
export const MAX_PAYLOAD_BYTES = 32 * 1024 * 1024 - 64 * 1024;
export const DEFAULT_PAYLOAD_BYTES = 16 * 1024 * 1024;
export const BATCH_FRAMES = 2048;

export type CaptureErrorCode = "unsupported" | "permission-denied" | "device" | "suspended" | "overflow" | "duration" | "cancelled" | "empty";

export class CaptureError extends Error {
  constructor(public readonly code: CaptureErrorCode) {
    super(`Microphone capture: ${code}`);
    this.name = "CaptureError";
  }
}

/** Linear interpolation at output timestamps; flush holds the final input sample. */
export class PcmConverter {
  private readonly bytes: Uint8Array;
  private readonly view: DataView;
  private inputFrames = 0;
  private outputFrames = 0;
  private previous = 0;
  private finished = false;

  constructor(
    readonly inputRate: number,
    readonly outputRate = 24000,
    readonly durationSeconds = MAX_DURATION_SECONDS,
    readonly payloadBytes = DEFAULT_PAYLOAD_BYTES,
  ) {
    if (!Number.isFinite(inputRate) || inputRate < 8000 || inputRate > 384000 ||
        !Number.isInteger(outputRate) || outputRate < 8000 || outputRate > 96000 ||
        !Number.isFinite(durationSeconds) || durationSeconds <= 0 || durationSeconds > MAX_DURATION_SECONDS ||
        !Number.isInteger(payloadBytes) || payloadBytes < 2 || payloadBytes > MAX_PAYLOAD_BYTES) {
      throw new RangeError("Invalid PCM capture limits");
    }
    this.bytes = new Uint8Array(Math.min(Math.floor(payloadBytes / 2) * 2, Math.ceil(durationSeconds * outputRate) * 2));
    this.view = new DataView(this.bytes.buffer);
  }

  push(samples: Float32Array, channels: number): void {
    if (this.finished) throw new CaptureError("device");
    if ((channels !== 1 && channels !== 2) || samples.length % channels !== 0 || samples.length > BATCH_FRAMES * channels) {
      throw new CaptureError("device");
    }
    const frames = samples.length / channels;
    if (this.inputFrames + frames > Math.floor(this.inputRate * this.durationSeconds)) throw new CaptureError("duration");
    for (let frame = 0; frame < frames; frame++) {
      let value = 0;
      for (let channel = 0; channel < channels; channel++) {
        const sample = samples[frame * channels + channel];
        if (!Number.isFinite(sample)) throw new CaptureError("device");
        value += sample / channels;
      }
      const index = this.inputFrames++;
      while (this.outputFrames * this.inputRate / this.outputRate <= index) {
        const position = this.outputFrames * this.inputRate / this.outputRate;
        const fraction = position - (index - 1);
        this.append(index === 0 ? value : this.previous + (value - this.previous) * fraction);
      }
      this.previous = value;
    }
  }

  finish(allowEmpty = false): Uint8Array {
    if (this.finished) throw new CaptureError("device");
    this.finished = true;
    if (!this.inputFrames && !allowEmpty) throw new CaptureError("empty");
    while (this.outputFrames * this.inputRate / this.outputRate < this.inputFrames) this.append(this.previous);
    return this.bytes.slice(0, this.outputFrames * 2);
  }

  private append(value: number): void {
    if ((this.outputFrames + 1) * 2 > this.bytes.length) throw new CaptureError("overflow");
    const clipped = Math.max(-1, Math.min(1, value));
    this.view.setInt16(this.outputFrames++ * 2, Math.round(clipped * (clipped < 0 ? 32768 : 32767)), true);
  }
}
