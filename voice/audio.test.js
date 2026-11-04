import { describe, it, expect } from "vitest";
import { decodeAudio, encodeAudio, sessionUpdate, unavailableTool, voiceFormat } from "./audio.js";

describe("NVIDIA voice protocol", () => {
  it("selects MLX's native formats and fixed model alias", () => {
    expect(voiceFormat("mlx_voicechat")).toEqual({ inputRate: 16000, outputRate: 22050, endEvent: "session.cancel" });
    expect(sessionUpdate("Hi", "mlx_voicechat").session).toEqual({ model: "agentrelay", system_prompt: "Hi", seed: 0 });
  });
  it("uses the 24k wire format, not the internal model sample rates", () => {
    const session = sessionUpdate("Keep it short.").session;
    expect(session.audio.input.format).toEqual({ type: "audio/pcm", rate: 24000 });
    expect(session.audio.output.format).toEqual(session.audio.input.format);
    expect(session.tools).toEqual([]);
    expect(() => sessionUpdate("Hello ☃")).toThrow("ASCII");
  });
  it("preserves signed PCM16 little-endian samples", () => {
    const buffer = new ArrayBuffer(6), view = new DataView(buffer);
    view.setInt16(0, -32768, true); view.setInt16(2, 0, true); view.setInt16(4, 32767, true);
    expect([...decodeAudio(encodeAudio(buffer))]).toEqual([-1, 0, 32767 / 32768]);
    expect(() => decodeAudio("AA==")).toThrow();
  });
  it("returns explicit unavailable results instead of executing model-generated code", () => {
    expect(unavailableTool("call_1").item).toEqual({ type: "function_call_output", call_id: "call_1",
      output: "No tools are enabled in this voice conversation." });
  });
});
