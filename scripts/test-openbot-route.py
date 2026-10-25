"""Opt-in live test for the dedicated OpenBot route (no third-party packages).

Runs synthetic prompts only. --cold-load unloads the selected idle model first;
it refuses to evict another model or interrupt active requests. This is a proxy
contract test, not proof of a complete OpenBot desktop agent turn.
"""
import argparse
import json
import time
import urllib.error
import urllib.request


def request(url, payload=None):
    data = None if payload is None else json.dumps(payload).encode()
    return urllib.request.urlopen(urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}), timeout=600)


def read_json(url, payload=None):
    with request(url, payload) as response:
        return json.load(response)


def stream(base, messages, **options):
    payload = {"model": "agentrelay", "messages": messages, "stream": True,
               "max_tokens": 4096, "chat_template_kwargs": {"enable_thinking": False},
               **options}
    with request(base + "/chat/completions", payload) as response:
        for line in response:
            if not line.startswith(b"data:"):
                continue
            data = line[5:].strip()
            if data == b"[DONE]":
                return
            event = json.loads(data)
            if "error" in event:
                raise RuntimeError(event["error"])
            yield event


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--proxy", default="http://127.0.0.1:38475")
    parser.add_argument("--cold-load", action="store_true")
    args = parser.parse_args()
    proxy = args.proxy.rstrip("/")
    base = proxy + "/clients/openbot/v1"
    discovered = read_json(base + "/models")
    assert [m["id"] for m in discovered["data"]] == ["agentrelay"]
    snapshot = read_json(proxy + "/api/v1/status")
    target = snapshot["openbot"]["selected_model"]
    assert target, "Select Route OpenBot before running the test"
    host_id, profile = target.split("/", 1)
    host = next(h for h in snapshot["hosts"] if h["id"] == host_id)
    assert host["active_requests"] == 0, "Host is busy; test deferred"
    if args.cold_load:
        assert host.get("loaded_model_id") in (None, profile), "Another model is loaded"
        result = read_json(proxy + "/api/v1/control/unload", {"host_id": host_id, "force": False})
        assert result["state"] in ("applied", "noop"), result
        for _ in range(30):
            idle = next(h for h in read_json(proxy + "/api/v1/status")["hosts"] if h["id"] == host_id)
            if not idle.get("loaded_model_id"):
                break
            time.sleep(1)
        else:
            raise RuntimeError("Unload did not become idle")

    started = time.monotonic()
    messages = [{"role": "user", "content": "Use relay_probe with value 21. After the tool returns, tell me its numeric result in your final answer."}]
    tools = [{"type": "function", "function": {"name": "relay_probe",
              "description": "Return twice the supplied integer.",
              "parameters": {"type": "object", "properties": {"value": {"type": "integer"}},
                             "required": ["value"], "additionalProperties": False}}}]
    calls, finishes, text = {}, [], ""
    first_delta = None
    for event in stream(base, messages, tools=tools,
                        tool_choice={"type": "function", "function": {"name": "relay_probe"}}):
        for choice in event.get("choices", []):
            delta = choice.get("delta", {})
            if delta and first_delta is None:
                first_delta = time.monotonic() - started
            text += delta.get("content") or ""
            for fragment in delta.get("tool_calls", []):
                call = calls.setdefault(fragment["index"], {"id": "", "type": "function",
                                        "function": {"name": "", "arguments": ""}})
                call["id"] += fragment.get("id") or ""
                for key in ("name", "arguments"):
                    call["function"][key] += fragment.get("function", {}).get(key) or ""
            if choice.get("finish_reason"):
                finishes.append(choice["finish_reason"])
    assert calls and "tool_calls" in finishes, (calls, finishes)
    messages.append({"role": "assistant", "content": text or None, "tool_calls": list(calls.values())})
    for call in calls.values():
        assert call["function"]["name"] == "relay_probe"
        value = json.loads(call["function"]["arguments"])["value"]
        assert value == 21
        messages.append({"role": "tool", "tool_call_id": call["id"], "content": str(2 * value)})
    answer, finish = "", None
    for event in stream(base, messages, tools=tools, tool_choice="none"):
        for choice in event.get("choices", []):
            answer += choice.get("delta", {}).get("content") or ""
            finish = choice.get("finish_reason") or finish
    assert finish == "stop" and "42" in answer, (finish, answer)

    cancelled = stream(base, [{"role": "user", "content": "Write the numbers 1 through 2000, one per line."}], max_tokens=4096)
    next(cancelled)
    cancelled.close()
    for _ in range(30):
        after = read_json(proxy + "/api/v1/status")
        host = next(h for h in after["hosts"] if h["id"] == host_id)
        if host["active_requests"] == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError("Active requests did not clear after cancellation")
    for client in ("hermes", "opencode", "codex"):
        assert after[client]["selected_model"] == snapshot[client]["selected_model"], client
    print(json.dumps({"model_discovery": "passed", "tool_round_trip": "passed",
                      "streaming": "passed", "cancellation": "passed",
                      "cold_load": args.cold_load, "target": target,
                      "first_delta_seconds": round(first_delta, 2),
                      "elapsed_seconds": round(time.monotonic() - started, 2)}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"HTTP {error.code}: {error.read(4096).decode(errors='replace')}") from error
