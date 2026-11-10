import source from "../../src/audio/live.worklet.js?raw";
import { runInNewContext } from "node:vm";
import { expect, test } from "vitest";

function processor() {
  const messages: Array<{ type: string; bytes?: Uint8Array }> = [];
  let Constructor: any;
  class Base { port = { onmessage: (_event: unknown) => {}, postMessage: (message: { type: string }) => messages.push(message) }; }
  runInNewContext(source, { AudioWorkletProcessor: Base, sampleRate: 48000, registerProcessor: (_name: string, value: unknown) => { Constructor = value; } });
  const instance = new Constructor({ processorOptions: { sampleRate: 24000 } });
  return { instance, messages, block: [[new Float32Array(128).fill(0.5)]] };
}
test("one live credit, bounded retention, explicit overflow instead of an audio Promise queue", () => {
  const { instance, messages, block } = processor();
  for (let index = 0; index < 1000; index++) instance.process(block);
  expect(messages.filter(message => message.type === "pcm")).toHaveLength(1);
  expect(messages[0].bytes!.length).toBe(4096);
  expect(messages.at(-1)?.type).toBe("error"); expect(instance.process(block)).toBe(false);
});
test("normal release flushes the resampler tail and waits for its acknowledgment before stopped", () => {
  const { instance, messages, block } = processor();
  instance.process(block); instance.port.onmessage({ data: { type: "stop" } });
  expect(messages.map(message => message.type)).toEqual(["pcm"]);
  expect(messages[0].bytes!.length).toBe(128);
  instance.port.onmessage({ data: { type: "ack" } });
  expect(messages.map(message => message.type)).toEqual(["pcm", "stopped"]);
  expect(instance.process(block)).toBe(false);
});
