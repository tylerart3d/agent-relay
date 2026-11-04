// A streaming area resampler handles device rates such as 44.1/48 kHz without
// dropping fractional samples at audio-worklet callback boundaries.
class Capture extends AudioWorkletProcessor {
  constructor(options) {
    super(); this.rate = options.processorOptions?.sampleRate === 16000 ? 16000 : 24000;
    this.samples = this.rate * 0.08;
    this.chunk = new ArrayBuffer(this.samples * 2); this.view = new DataView(this.chunk);
    this.index = 0; this.weight = 0; this.total = 0;
  }
  process(inputs) {
    const input = inputs[0]?.[0];
    if (!input) return true;
    for (const sample of input) {
      let remaining = this.rate / sampleRate;
      while (remaining > 1e-9) {
        const part = Math.min(remaining, 1 - this.weight);
        this.total += sample * part; this.weight += part; remaining -= part;
        if (this.weight >= 1 - 1e-9) {
          const value = Math.max(-1, Math.min(1, this.total));
          this.view.setInt16(this.index * 2, Math.round(value * (value < 0 ? 32768 : 32767)), true);
          this.index++; this.weight = 0; this.total = 0;
          if (this.index === this.samples) {
            this.port.postMessage(this.chunk, [this.chunk]);
            this.chunk = new ArrayBuffer(this.samples * 2); this.view = new DataView(this.chunk); this.index = 0;
          }
        }
      }
    }
    return true;
  }
}
registerProcessor("voice-capture", Capture);
