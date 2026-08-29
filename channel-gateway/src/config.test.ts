import { describe, expect, it } from "vitest";
import { loadConfig, normalizeSender } from "./config.js";

describe("gateway config", () => {
  it("normalizes allowlisted phone numbers", () => {
    expect(normalizeSender("+1 (555) 123-4567")).toBe("+15551234567");
  });

  it("loads required Photon credentials without exposing defaults", () => {
    const config = loadConfig({
      PHOTON_PROJECT_ID: "project",
      PHOTON_PROJECT_SECRET: "secret",
      AGENT_RELAY_ALLOWED_SENDERS: "+1 555 123 4567, Friend@Example.com",
    });
    expect(config.agentRelayEndpoint).toBe("http://127.0.0.1:38475");
    expect(config.adapterId).toBe("photon-imessage");
    expect(config.allowedSenders).toEqual(new Set(["+15551234567", "friend@example.com"]));
  });
});
