import { voiceFormat, encodeAudio, decodeAudio, sessionUpdate, unavailableTool } from "./audio.js";

const element = (id) => document.getElementById(id);
const start = element("start"), stop = element("stop"), status = element("status"), instructions = element("instructions");
const params = new URLSearchParams(location.search);
const host = params.get("host"), model = params.get("model");
const endpoint = `/api/voice/${encodeURIComponent(host)}/${encodeURIComponent(model)}`;
let current = null, targetReady = false, protocol = "nemotron_realtime";

function clearPlayback(session) {
  for (const node of session.playing) { try { node.stop(); } catch { /* already ended */ } }
  session.playing.clear(); session.nextAudio = 0;
}
function end(message = "Conversation ended.") {
  const session = current; current = null;
  if (session) {
    session.abort.abort();
    if (session.socket?.readyState === WebSocket.OPEN) {
      session.socket.send(JSON.stringify({ type: voiceFormat(protocol).endEvent, event_id: crypto.randomUUID() }));
    }
    session.socket?.close(); session.capture?.disconnect(); session.source?.disconnect();
    session.stream?.getTracks().forEach((track) => track.stop());
    clearPlayback(session); void session.audio.close();
  }
  start.disabled = !targetReady; stop.disabled = true; instructions.disabled = false; status.textContent = message;
}
function transcript(session, role, id, text, delta) {
  const key = `${role}:${id || "current"}`;
  let row = session.rows.get(key);
  if (!row) {
    row = document.createElement("p"); session.rows.set(key, row); element("transcript").append(row);
    while (session.rows.size > 12) {
      const first = session.rows.keys().next().value; session.rows.get(first).remove(); session.rows.delete(first);
    }
  }
  row.dataset.text = ((delta ? row.dataset.text || "" : "") + text).slice(-8000);
  row.textContent = `${role}: ${row.dataset.text}`;
}
function receive(session, data, update) {
  if (current !== session) return;
  try {
    const event = JSON.parse(data);
    switch (event.type) {
      case "session.created": session.socket.send(JSON.stringify(update)); break;
      case "session.updated": session.ready = true; status.textContent = "Listening. Speak naturally; you can interrupt."; break;
      case "input_audio_buffer.speech_started": clearPlayback(session); break;
      case "response.audio.delta":
      case "response.output_audio.delta": {
        const samples = decodeAudio(event.delta);
        if (!samples.length) break;
        const now = session.audio.currentTime;
        if (session.nextAudio - now > 8) throw new Error("Audio playback fell behind. End and reconnect when your device is ready.");
        const buffer = session.audio.createBuffer(1, samples.length, voiceFormat(protocol).outputRate); buffer.copyToChannel(samples, 0);
        const node = session.audio.createBufferSource(); node.buffer = buffer; node.connect(session.audio.destination);
        session.playing.add(node); node.onended = () => { session.playing.delete(node); node.disconnect(); };
        const at = Math.max(now + 0.015, session.nextAudio); node.start(at); session.nextAudio = at + buffer.duration;
        break;
      }
      case "response.output_audio_transcript.delta": transcript(session, "Agent", event.item_id || event.response_id, event.delta || "", true); break;
      case "response.output_audio_transcript.done": transcript(session, "Agent", event.item_id || event.response_id, event.transcript || "", false); break;
      case "response.text.delta": transcript(session, "Agent", "mlx", event.text ?? event.delta ?? "", !event.text); break;
      case "conversation.item.input_audio_transcription.delta": transcript(session, "You", event.item_id || "mlx", event.transcript ?? event.delta ?? "", !event.transcript); break;
      case "conversation.item.input_audio_transcription.completed": transcript(session, "You", event.item_id, event.transcript || "", false); break;
      case "response.function_call_arguments.done":
        session.socket.send(JSON.stringify(unavailableTool(event.call_id))); break;
      case "error": end(event.error?.message || "The voice runtime reported an error."); break;
      case "session.end": end("Voice session finished."); break;
      case "response.cancelled": end("Voice session cancelled."); break;
    }
  } catch (error) { end(error.message || "Invalid voice response"); }
}
start.addEventListener("click", async () => {
  if (current) return;
  let session;
  try {
    const update = sessionUpdate(instructions.value, protocol);
    const audio = new AudioContext();
    session = { audio, abort: new AbortController(), playing: new Set(), rows: new Map(), nextAudio: 0, ready: false };
    current = session; start.disabled = true; stop.disabled = false; instructions.disabled = true;
    element("transcript").replaceChildren(); status.textContent = "Requesting microphone access…";
    await audio.resume();
    const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } });
    if (current !== session) { stream.getTracks().forEach((track) => track.stop()); return; }
    session.stream = stream;
    await audio.audioWorklet.addModule("/voice/capture.js");
    if (current !== session) return;
    status.textContent = "Preparing voice model. A cold start may take several minutes…";
    const response = await fetch(`${endpoint}/prepare`, { method: "POST", signal: session.abort.signal });
    if (!response.ok) {
      const body = await response.json().catch(() => ({}));
      throw new Error(body.error || `Voice setup failed (${response.status}). Check the host and runtime.`);
    }
    if (current !== session) return;
    const socket = new WebSocket(`${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}${endpoint}/realtime`);
    session.socket = socket;
    socket.onmessage = (event) => receive(session, event.data, update);
    socket.onerror = () => { if (current === session) end("Voice connection failed. Check that the model is ready and no other model replaced it."); };
    socket.onclose = () => { if (current === session) end("Voice connection closed. Start again when the host is available."); };
    session.source = audio.createMediaStreamSource(stream);
    session.capture = new AudioWorkletNode(audio, "voice-capture", { channelCount: 1, processorOptions: { sampleRate: voiceFormat(protocol).inputRate } });
    session.capture.port.onmessage = ({ data }) => {
      if (current !== session || !session.ready || socket.readyState !== WebSocket.OPEN) return;
      if (socket.bufferedAmount > 256 * 1024) { end("Network audio fell behind. Reconnect when the connection is stable."); return; }
      socket.send(JSON.stringify({ type: "input_audio_buffer.append", event_id: crypto.randomUUID(), sample_rate: voiceFormat(protocol).inputRate, audio: encodeAudio(data) }));
    };
    // The worklet emits silence, so connecting its output keeps capture scheduled
    // without playing the microphone back through the speakers.
    session.source.connect(session.capture); session.capture.connect(audio.destination);
    status.textContent = "Connecting voice session…";
  } catch (error) { if (!session || current === session) end(error.message || "Could not start voice"); }
});
stop.addEventListener("click", () => end());
window.addEventListener("pagehide", () => end());

try {
  const response = await fetch("/api/voice/models");
  if (!response.ok) throw new Error("Agent Relay could not list voice profiles.");
  const { models } = await response.json();
  const target = models.find((item) => item.host_id === host && item.model_id === model);
  if (!target) throw new Error("This voice profile is unavailable. Open Voice again from Agent Relay.");
  element("target").textContent = `${target.host_name} · ${target.name}`;
  protocol = target.protocol;
  if (!target.online) throw new Error("This host is offline. Reopen Voice when it is available.");
  if (!navigator.mediaDevices?.getUserMedia) throw new Error("Open this page on localhost in a browser with microphone support.");
  targetReady = true; start.disabled = false;
} catch (error) { status.textContent = error.message; }
