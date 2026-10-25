"""Opt-in disk-state benchmark. Starts/stops only a dedicated test server.

The private JSON spec supplies argv, cwd, endpoint, model and cache_dir. In argv,
{cache_dir} is expanded to that directory. No live Relay configuration is changed.
Use an unused loopback port and an otherwise idle accelerator. Logs and cache files
contain model state and must remain outside Git. Requires Python 3.10+.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request


def request(base, path, payload=None, timeout=300):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


def stop(child):
    if child is None or child.poll() is not None:
        return
    if os.name == "nt":
        # Kill the tree of our own Popen child only, never by image name.
        subprocess.run(["taskkill", "/PID", str(child.pid), "/T", "/F"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    else:
        import signal
        os.killpg(child.pid, signal.SIGTERM)
    try:
        child.wait(timeout=15)
    except subprocess.TimeoutExpired:
        if os.name != "nt":
            os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=15)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("spec", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    spec = json.loads(args.spec.read_text(encoding="utf-8-sig"))
    base = spec["endpoint"].rstrip("/")
    url = urllib.parse.urlsplit(base)
    if url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port:
        raise ValueError("The benchmark requires an explicit 127.0.0.1 HTTP port")
    with socket.socket() as sock:
        sock.bind((url.hostname, url.port))  # Refuse an existing listener.
    cache = Path(spec["cache_dir"]).resolve()
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    argv = [arg.replace("{cache_dir}", str(cache)) for arg in spec["argv"]]
    child = None
    report = {"model": spec["model"], "phases": {}}
    # Optional native core exercises the exact retention and slot orchestration
    # implemented for Relay, across separate diagnostic processes. Its identity
    # is a benchmark-only configuration key; production must fingerprint models
    # and the actual engine binary too (a Python launcher's hash is not enough).
    compatibility = hashlib.sha256(json.dumps(argv).encode()).hexdigest()

    def snapshot_action(action):
        if "probe_binary" in spec:
            result = subprocess.run([
                spec["probe_binary"], action, str(cache), base,
                spec["model"], compatibility,
            ], check=True, text=True, capture_output=True, timeout=150)
            return json.loads(result.stdout)
        return request(base, f"/slots/0?action={action}", {"filename": "benchmark.bin"})
    prefix = "\n".join(
        f"Rule {i}: verify precondition {(i * 7) % 89}, record result {(i * 13) % 83}."
        for i in range(spec.get("rules", 150)))
    messages = [{"role": "system", "content": "Synthetic cache benchmark. " + prefix},
                {"role": "user", "content": "Reply with OK."}]

    def generate(name, content):
        started = time.monotonic()
        result = request(base, "/v1/chat/completions", {
            "model": spec["model"], "messages": messages[:-1] + [
                {"role": "user", "content": content}],
            "max_tokens": 32, "temperature": 0, "stream": False,
            "cache_prompt": True, "chat_template_kwargs": {"enable_thinking": False},
            "reasoning_budget": 0,
        })
        usage = result.get("usage", {})
        timing = result.get("timings", {})
        cached = timing.get("cache_n", usage.get("prompt_tokens_details", {}).get("cached_tokens"))
        phase = {"seconds": round(time.monotonic() - started, 3), "usage": usage,
                 "timings": timing, "cached_tokens": cached,
                 "finish_reason": result["choices"][0].get("finish_reason")}
        report["phases"][name] = phase
        print(json.dumps({name: phase}), flush=True)
        return cached

    def start(log):
        proc = subprocess.Popen(argv, cwd=spec.get("cwd"), stdout=log, stderr=log,
                                start_new_session=os.name != "nt",
                                creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
        deadline = time.monotonic() + spec.get("startup_timeout", 600)
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError(f"Test server exited with {proc.returncode}; inspect its log")
            try:
                request(base, "/health", timeout=2)
                return proc
            except (OSError, ValueError):
                time.sleep(1)
        stop(proc)
        raise TimeoutError("Test server did not become ready")

    try:
        with (cache / "benchmark-first.log").open("wb") as log:
            child = start(log)
            report["first_pid"] = child.pid
            generate("cold", "Reply with OK.")
            warm = generate("warm", "Reply with READY.")
            report["save"] = snapshot_action("save")
            print(json.dumps({"save": report["save"]}), flush=True)
            stop(child)
        with (cache / "benchmark-restarted.log").open("wb") as log:
            child = start(log)
            report["second_pid"] = child.pid
            report["restore"] = snapshot_action("restore")
            restored = generate("restored", "Reply with DONE.")
            assert warm is not None and warm > 0, "Runtime did not report warm prefix reuse"
            assert restored is not None and restored >= warm * 0.9, "Restored prefix was not substantially reused"
            report["passed"] = True
    except Exception as error:
        report["passed"] = False
        report["error"] = str(error)
        raise
    finally:
        stop(child)
        args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"passed": report.get("passed", False)}), flush=True)


if __name__ == "__main__":
    main()
