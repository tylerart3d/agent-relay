"""POSIX integration test using actual llama-swap and a synthetic runtime; no GPU.

python test_lifecycle.py --swap /path/llama-swap --probe /path/kv-cache-probe
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request


def http(base, path, body=None, timeout=20):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_until(predicate, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, ValueError):
            pass
        time.sleep(0.1)
    raise AssertionError("Timed out waiting for expected state")


def fake_runtime():
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    from urllib.parse import urlsplit, parse_qs
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int)
    parser.add_argument("--slot-save-path", type=Path)
    args, _ = parser.parse_known_args()
    state = {"tokens": 0, "busy": False, "has_task": False}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def reply(self, body):
            data = json.dumps(body).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        def do_GET(self):
            if self.path == "/slots":
                slot = {"id": 0, "is_processing": state["busy"]}
                if state["has_task"]:
                    slot["n_prompt_tokens"] = state["tokens"]
                self.reply([slot])
            else:
                self.reply({"status": "ok"})
        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            path = urlsplit(self.path)
            if path.path == "/slots/0":
                filename = data["filename"]
                target = args.slot_save_path / filename
                if parse_qs(path.query)["action"] == ["save"]:
                    target.write_text(str(state["tokens"]))
                    self.reply({"id_slot": 0, "filename": filename, "n_saved": state["tokens"], "n_written": target.stat().st_size})
                else:
                    state["tokens"] = int(target.read_text())
                    self.reply({"id_slot": 0, "filename": filename, "n_restored": state["tokens"]})
                return
            cached = state["tokens"]
            state["busy"] = True
            state["has_task"] = True
            if data.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                try:
                    for _ in range(14):
                        self.wfile.write(b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n')
                        self.wfile.flush()
                        time.sleep(0.5)
                    self.wfile.write(b'data: [DONE]\n\n')
                except (BrokenPipeError, ConnectionResetError):
                    pass
                finally:
                    state["tokens"] = 1200
                    state["busy"] = False
            else:
                state["tokens"] = 1200
                state["busy"] = False
                self.reply({"choices": [{"message": {"content": "OK"}, "finish_reason": "stop"}], "timings": {"cache_n": cached}})
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swap", required=True)
    parser.add_argument("--probe", required=True)
    args = parser.parse_args()
    assert os.name == "posix", "POSIX lifecycle test only"
    wrapper = Path(__file__).with_name("llama_cache.py").resolve()
    spec = importlib.util.spec_from_file_location("wrapper", wrapper)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    runtime = module.runtime_hash(sys.executable)
    with tempfile.TemporaryDirectory(prefix="relay-kv-lifecycle-") as temp:
        root = Path(temp)
        cache = root / "cache"
        base = f"http://127.0.0.1:{free_port()}"
        command = [sys.executable, str(wrapper), "--listen-port", "${PORT}", "--cache-dir", str(cache),
                   "--profile", "mock-qwen", "--probe", args.probe, "--verified-runtime", runtime, "--",
                   sys.executable, str(Path(__file__).resolve()), "--fake", "--host", "127.0.0.1",
                   "--port", "{upstream_port}", "--slot-save-path", "{cache_dir}", "--parallel", "1"]
        config = {"healthCheckTimeout": 30, "startPort": free_port(), "models": {"mock-qwen": {
            "cmd": shlex.join(command), "cmdStop": "/bin/kill -TERM ${PID}", "proxy": "http://127.0.0.1:${PORT}",
            "ttl": 3, "unloadTimeout": 10}}}
        (root / "config.yaml").write_text(json.dumps(config))
        with (root / "swap.log").open("wb") as log:
            swap = subprocess.Popen([args.swap, "--config", str(root / "config.yaml"), "--listen", base[7:]], stdout=log, stderr=log)
            try:
                wait_until(lambda: http(base, "/v1/models"))
                payload = {"model": "mock-qwen", "messages": [{"role": "user", "content": "test"}]}
                cold = http(base, "/v1/chat/completions", payload)
                assert cold["timings"]["cache_n"] == 0
                wait_until(lambda: not http(base, "/running")["running"])
                warm = http(base, "/v1/chat/completions", payload)
                assert warm["timings"]["cache_n"] == 1200, "TTL unload must save, reload must restore before inference"
                # A same-size corrupt snapshot passes file-size checks, but a
                # failed native restore must restart cleanly before admission.
                http(base, "/api/models/unload", {})
                wait_until(lambda: not http(base, "/running")["running"])
                snapshot, = cache.glob("*.bin")
                snapshot.write_bytes(b"oops")
                recovered = http(base, "/v1/chat/completions", payload)
                assert recovered["timings"]["cache_n"] == 0, "Rejected restore must serve from a fresh runtime"
                # A live stream exceeds ttl without eviction.
                request = urllib.request.Request(base + "/v1/chat/completions", data=json.dumps({**payload,"stream":True}).encode(), headers={"Content-Type":"application/json"})
                with urllib.request.urlopen(request, timeout=15) as stream:
                    assert b"data:" in stream.readline()
                    time.sleep(4)
                    assert http(base, "/running")["running"][0]["state"] == "ready"
                    # Close mid-stream; cancellation must reach the native slot.
                wait_until(lambda: not http(base, "/upstream/mock-qwen/slots")[0]["is_processing"], seconds=5)
                started = time.monotonic()
                http(base, "/api/models/unload", {})
                wait_until(lambda: not http(base, "/running")["running"])
                assert time.monotonic() - started < 10
                catalog = json.loads(subprocess.check_output([args.probe, "list", str(cache),
                    "http://127.0.0.1:1", "mock-qwen", "inspection"], text=True))
                assert len(catalog["snapshots"]) == 1 and catalog["snapshots"][0]["tokens"] == 1200
                print(json.dumps({"passed": True, "ttl_save_restore": True, "stream_across_ttl": True,
                                  "cancellation": True, "corrupt_restore_cold_fallback": True}))
            except BaseException:
                log.flush()
                print((root / "swap.log").read_text()[-6000:])
                raise
            finally:
                swap.terminate()
                try:
                    swap.wait(timeout=12)
                except subprocess.TimeoutExpired:
                    swap.kill()
                    swap.wait()


if __name__ == "__main__":
    if "--fake" in sys.argv:
        fake_runtime()
    else:
        main()
