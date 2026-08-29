import { describe, expect, it } from "vitest";
import { formatCommandResponse } from "./format.js";

describe("formatCommandResponse", () => {
  it("uses the mobile success message without exposing the technical route", () => {
    expect(formatCommandResponse({
      ok: true,
      handled: true,
      command: "attach",
      mobile_message: "Connected Tower Defense. Send your next message to continue.",
      route: {
        channel: "imessage",
        account_id: "personal",
        conversation_id: "chat",
        session_id: 1,
        harness: "open_code",
        harness_host_id: "m1-pro",
        host_id: "workstation",
        model_id: "internal-model-id",
        project: "/Users/example/private-project",
        updated_at_ms: 1,
      },
    })).toBe("Connected Tower Defense. Send your next message to continue.");
  });
  it("shows portable context handoff state", () => {
    expect(
      formatCommandResponse({
        ok: true,
        handled: true,
        command: "move",
        message: "Moved",
        route: {
          channel: "photon",
          account_id: "personal",
          conversation_id: "brainstorm",
          session_id: 2,
          harness: "pi",
          harness_host_id: "workstation",
          host_id: "air-m4",
          model_id: "qwen",
          project: "agent-relay",
          handoff_from_session_id: 1,
          handoff_status: "pending",
          native_archive_status: "pending",
          updated_at_ms: 2,
        },
      }),
    ).toContain("context transfer pending");
    expect(
      formatCommandResponse({
        ok: true,
        handled: true,
        command: "move",
        route: {
          channel: "photon",
          account_id: "personal",
          conversation_id: "brainstorm",
          session_id: 2,
          harness: "pi",
          host_id: "air-m4",
          model_id: "qwen",
          native_archive_status: "completed",
          updated_at_ms: 3,
        },
      }),
    ).toContain("source archived");
  });
});
