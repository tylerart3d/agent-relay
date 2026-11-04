import { readFileSync } from "node:fs";
import vm from "node:vm";
import { describe, it, expect } from "vitest";

describe("continuous microphone resampling", () => {
  for (const rate of [44100, 48000]) for (const target of [16000, 24000]) {
    it(`keeps exact 80ms frames from ${rate} to ${target} across callbacks`, () => {
      const frames = [];
      let Processor;
      vm.runInNewContext(readFileSync(new URL("./capture.js", import.meta.url), "utf8"), {
        AudioWorkletProcessor: class { port = { postMessage: (buffer) => frames.push(buffer) }; },
        registerProcessor: (_, value) => { Processor = value; }, sampleRate: rate,
      });
      const capture = new Processor({ processorOptions: { sampleRate: target } });
      for (let offset = 0; offset < rate * 2; offset += 128) {
        capture.process([[new Float32Array(Math.min(128, rate * 2 - offset)).fill(-1)]]);
      }
      expect(frames).toHaveLength(25); // Exactly 2 seconds, no boundary drift.
      expect(frames.every((frame) => frame.byteLength === target * .08 * 2)).toBe(true);
      for (const frame of frames) {
        const view = new DataView(frame);
        expect(view.getInt16(0, true)).toBe(-32768);
        expect(view.getInt16(frame.byteLength - 2, true)).toBe(-32768);
      }
    });
  }
});
