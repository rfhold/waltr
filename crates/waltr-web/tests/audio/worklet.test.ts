import workletSource from "../../src/audio/capture.worklet.js?raw";
import { runInNewContext } from "node:vm";
import { describe, expect, it } from "vitest";

type Message = { type: string; code?: string; channels?: number; samples?: Float32Array };
function processor(durationSeconds = 300) {
  const messages: Message[] = [];
  let Constructor: new (options: unknown) => {
    port: { onmessage: (event: { data: { type: string } }) => void };
    process(inputs: Float32Array[][]): boolean;
  };
  class Base {
    port = { onmessage: () => {}, postMessage: (data: Message) => messages.push(data) };
  }
  runInNewContext(workletSource, {
    AudioWorkletProcessor: Base, sampleRate: 48000,
    registerProcessor: (_name: string, value: typeof Constructor) => { Constructor = value; },
  });
  const instance = new Constructor!({ processorOptions: { durationSeconds } });
  const block = [new Float32Array(128).fill(0.25), new Float32Array(128).fill(-0.5)];
  return { instance, messages, block };
}

describe("bounded worklet transport", () => {
  it("interleaves stereo, flushes tails before stopped, and terminates", () => {
    const { instance, block, messages } = processor();
    instance.process([block]);
    instance.port.onmessage({ data: { type: "stop" } });
    expect(messages.map(value => value.type)).toEqual(["samples", "stopped"]);
    expect(messages[0].channels).toBe(2);
    expect(messages[0].samples?.length).toBe(256);
    expect([...messages[0].samples!.slice(0, 4)]).toEqual([0.25, -0.5, 0.25, -0.5]);
    expect(instance.process([block])).toBe(false);
    instance.port.onmessage({ data: { type: "stop" } });
    expect(messages).toHaveLength(2);
  });

  it("caps unacknowledged audio at eight finite batches, then explicitly overflows", () => {
    const { instance, block, messages } = processor();
    for (let index = 0; index < 16 * 9; index++) instance.process([block]);
    const batches = messages.filter(value => value.type === "samples");
    expect(batches).toHaveLength(8);
    expect(batches.reduce((sum, value) => sum + value.samples!.byteLength, 0)).toBe(128 * 1024);
    expect(messages.at(-1)).toEqual({ type: "error", code: "overflow" });
    expect(instance.process([block])).toBe(false);
  });

  it("returns credits only by explicit acknowledgement", () => {
    const { instance, block, messages } = processor();
    for (let batch = 0; batch < 30; batch++) {
      for (let quantum = 0; quantum < 16; quantum++) expect(instance.process([block])).toBe(true);
      instance.port.onmessage({ data: { type: "ack" } });
    }
    expect(messages).toHaveLength(30);
    expect(messages.every(value => value.type === "samples")).toBe(true);
  });

  it("enforces source-clock duration even when the main thread cannot run its timer", () => {
    const { instance, block, messages } = processor(128 / 48000);
    expect(instance.process([block])).toBe(true);
    expect(instance.process([block])).toBe(false);
    expect(messages.at(-1)).toEqual({ type: "error", code: "duration" });
  });
});
