// NVIDIA's wire format is 24 kHz mono PCM16LE. The model's native rates
// (16 kHz / 22.05 kHz) are converted inside its server, not by Relay.
export const SAMPLE_RATE = 24000;
export const CHUNK_SAMPLES = 1920; // 80 ms
export function encodeAudio(buffer) {
  const bytes = new Uint8Array(buffer);
  let text = "";
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text);
}
export function decodeAudio(base64) {
  const text = atob(base64);
  if (text.length % 2 || text.length > 256 * 1024) throw new Error("Invalid voice audio frame");
  const bytes = Uint8Array.from(text, (char) => char.charCodeAt(0));
  const view = new DataView(bytes.buffer);
  return Float32Array.from({ length: bytes.length / 2 }, (_, index) => view.getInt16(index * 2, true) / 32768);
}
export function voiceFormat(protocol) {
  return protocol === "mlx_voicechat"
    ? { inputRate: 16000, outputRate: 22050, endEvent: "session.cancel" }
    : { inputRate: 24000, outputRate: 24000, endEvent: "session.close" };
}
export function sessionUpdate(instructions, protocol = "nemotron_realtime") {
  if (!/^[\x00-\x7F]*$/.test(instructions)) throw new Error("Voice instructions must use ASCII characters for this model.");
  if (protocol === "mlx_voicechat") return {
    type: "session.update", event_id: crypto.randomUUID(),
    session: { model: "agentrelay", system_prompt: instructions.trim() || undefined, seed: 0 },
  };
  return {
    type: "session.update", event_id: crypto.randomUUID(),
    session: { audio: {
      input: { format: { type: "audio/pcm", rate: SAMPLE_RATE } },
      output: { format: { type: "audio/pcm", rate: SAMPLE_RATE } },
    }, instructions: instructions.trim() || null, tools: [] },
  };
}
export function unavailableTool(callId) {
  return { type: "conversation.item.create", event_id: crypto.randomUUID(), item: {
    type: "function_call_output", call_id: callId,
    output: "No tools are enabled in this voice conversation.",
  } };
}
