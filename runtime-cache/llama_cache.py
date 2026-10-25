"""Opt-in POSIX llama-swap adapter for a capability-tested llama.cpp build.

cmdStop MUST signal only this wrapper PID, e.g. /bin/kill -TERM ${PID}.
The child remains in llama-swap's process group for final forced cleanup.
No HTTP rewriting: the public socket opens only after optional restoration.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import socketserver
import subprocess
import threading
import time
from typing import Any, Optional
import urllib.request


def emit(event: str, **fields: Any) -> None:
    print(json.dumps({"kv_cache": event, **fields}), flush=True)


def read_json(base: str, path: str, timeout: float = 3) -> Any:
    with urllib.request.urlopen(base + path, timeout=timeout) as response:
        return json.load(response)


def file_digest(path: Path, full: bool = False) -> str:
    path = Path(path).resolve(strict=True)
    stat = path.stat()
    digest = hashlib.sha256()
    digest.update(str(stat.st_size).encode())
    # Metadata makes ordinary in-place replacements conservative misses. Large
    # immutable weight shards are sampled; runtime binaries are hashed in full.
    if not full:
        digest.update(str(stat.st_mtime_ns).encode())
    with path.open("rb") as source:
        if full:
            while block := source.read(1024 * 1024):
                digest.update(block)
        else:
            digest.update(source.read(1024 * 1024))
            source.seek(max(0, stat.st_size - 1024 * 1024))
            digest.update(source.read(1024 * 1024))
    return digest.hexdigest()


def runtime_hash(path: str) -> str:
    digest = hashlib.sha256()
    executable = Path(path).resolve(strict=True)
    # Runtime identity includes sibling dylibs (the executable is often tiny).
    files = {executable, *executable.parent.glob("*.dylib")}
    for item in sorted({p.resolve() for p in files}):
        digest.update(item.name.encode())
        digest.update(file_digest(item, full=True).encode())
    return digest.hexdigest()


def compatibility(command: list[str], runtime: str) -> str:
    digest = hashlib.sha256(json.dumps(command, ensure_ascii=False).encode())
    digest.update(runtime.encode())
    files = set()
    for argument in command[1:]:
        path = Path(argument)
        if path.is_file():
            files.add(path.resolve())
            match = re.match(r"^(.*)-\d{5}-of-(\d{5})\.gguf$", path.name)
            if match:
                for number in range(1, int(match[2]) + 1):
                    shard = path.with_name(f"{match[1]}-{number:05d}-of-{match[2]}.gguf")
                    files.add(shard.resolve(strict=True))
    for path in sorted(files):
        digest.update(str(path).encode())
        digest.update(file_digest(path).encode())
    return digest.hexdigest()


class Proxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, port: int, upstream: tuple[str, int]) -> None:
        self.upstream = upstream
        self.connections = set()
        self.lock = threading.Lock()
        self.stopping = False
        super().__init__(("127.0.0.1", port), Pipe)

    def close_connections(self) -> None:
        with self.lock:
            self.stopping = True
            for connection in self.connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()


class Pipe(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        upstream = None
        try:
            upstream = socket.create_connection(self.server.upstream, timeout=3)
            with self.server.lock:
                if self.server.stopping:
                    return
                self.server.connections.update((self.request, upstream))
            peers = {self.request: upstream, upstream: self.request}
            for connection in peers:
                connection.settimeout(5)
            while peers and not self.server.stopping:
                readable, _, _ = select.select(list(peers), [], [], 1)
                for connection in readable:
                    data = connection.recv(65536)
                    if not data:
                        # Propagate cancellation/EOF immediately. Do not keep a
                        # model request alive after its consumer disconnected.
                        return
                    peers[connection].sendall(data)
        except (OSError, ValueError):
            pass
        finally:
            with self.server.lock:
                self.server.connections.discard(self.request)
                if upstream:
                    self.server.connections.discard(upstream)
            if upstream:
                upstream.close()


def run_probe(args: argparse.Namespace, action: str, endpoint: str, fingerprint: str,
              timeout: float = 8) -> dict:
    result = subprocess.run([args.probe, action, args.cache_dir, endpoint,
                             args.profile, fingerprint], capture_output=True,
                            text=True, timeout=timeout)
    if result.returncode:
        # Probe diagnostics contain no request content, but avoid logging local
        # paths or arbitrary runtime responses through llama-swap's event feed.
        raise RuntimeError(f"cache {action} failed (exit {result.returncode})")
    return json.loads(result.stdout)


def terminate(child: Optional[subprocess.Popen]) -> None:
    if child is None or child.poll() is not None:
        return
    child.terminate()
    try:
        child.wait(timeout=2)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=2)


def validate_command(command: list[str]) -> None:
    if not command or not Path(command[0]).is_absolute():
        raise ValueError("An absolute native runtime executable is required after --")
    for flag, expected in [("--host", "127.0.0.1"), ("--port", "{upstream_port}"),
                           ("--slot-save-path", "{cache_dir}"), ("--parallel", "1")]:
        positions = [i for i, arg in enumerate(command) if arg == flag]
        if len(positions) != 1 or command[positions[0] + 1:positions[0] + 2] != [expected]:
            raise ValueError(f"Native command must contain exactly {flag} {expected}")
    for alias in ["-np", "-c", "--no-slots"]:
        if alias in command:
            raise ValueError(f"Unsupported cache profile flag: {alias}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen-port", type=int)
    parser.add_argument("--cache-dir", required=True)
    parser.add_argument("--profile", default="maintenance")
    parser.add_argument("--probe", required=True)
    parser.add_argument("--verified-runtime")
    parser.add_argument("--maintenance", action="store_true")
    parser.add_argument("--startup-timeout", type=int, default=600)
    parser.add_argument("--save-timeout", type=float, default=7)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("This launch adapter is currently supported on POSIX hosts only")
    import fcntl
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not args.maintenance:
        validate_command(command)
        if not args.listen_port or not args.verified_runtime:
            parser.error("listen-port and verified-runtime are required to serve a model")
    if not 1 <= args.save_timeout <= 7:
        parser.error("save-timeout must be between 1 and 7 seconds (Relay unload deadline)")
    root = Path(args.cache_dir).expanduser()
    if root.is_symlink():
        parser.error("Cache directory must not be a symlink")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    args.cache_dir = str(root.resolve())
    lock = (root / "runtime.lock").open("a+")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        if args.maintenance:
            return  # The active wrapper performs its own hourly maintenance.
        raise
    marker = root / "runtime.json"
    if marker.exists():
        previous = json.loads(marker.read_text())
        try:
            os.kill(previous["pid"], 0)
        except ProcessLookupError:
            marker.unlink()
        else:
            raise RuntimeError("A previous cache runtime may still be alive; refusing to overlap it")
    if args.maintenance:
        run_probe(args, "recover", "http://127.0.0.1:1", "maintenance")
        return
    actual_runtime = runtime_hash(command[0])
    if actual_runtime != args.verified_runtime:
        raise RuntimeError("Runtime has changed; repeat the cache-reuse benchmark before enabling it")
    fingerprint = compatibility(command, actual_runtime)
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        upstream_port = reserve.getsockname()[1]
    endpoint = f"http://127.0.0.1:{upstream_port}"
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    child = None
    proxy = None
    restored = False

    def start_native(native: list[str]) -> subprocess.Popen:
        proc = subprocess.Popen(native)
        deadline = time.monotonic() + args.startup_timeout
        try:
            marker.write_text(json.dumps({"pid": proc.pid}))
            while time.monotonic() < deadline:
                if stop.is_set():
                    raise InterruptedError("Stopped during model loading")
                if proc.poll() is not None:
                    raise RuntimeError("Native runtime exited during startup")
                try:
                    read_json(endpoint, "/health")
                    return proc
                except (OSError, ValueError):
                    stop.wait(0.5)
            raise TimeoutError("Native runtime did not become ready")
        except BaseException:
            terminate(proc)
            raise

    try:
        # The runtime lock plus absent/dead previous child is the recovery barrier.
        run_probe(args, "recover", endpoint, fingerprint)
        native = [x.replace("{upstream_port}", str(upstream_port)).replace("{cache_dir}", args.cache_dir)
                  for x in command]
        child = start_native(native)
        catalog = run_probe(args, "prune", endpoint, fingerprint)
        candidates = [entry for entry in catalog["snapshots"] if entry["ready"]
                      and entry["profile"] == args.profile and entry["compatibility"] == fingerprint]
        if candidates:
            try:
                result = run_probe(args, "restore-fresh", endpoint, fingerprint, timeout=120)
                emit("restored", tokens=result["snapshot"]["tokens"])
                restored = True
            except Exception:
                # A partial restore must never serve inference. Terminate before
                # discarding the invalid entry and doing one clean cold start.
                terminate(child)
                run_probe(args, "discard:" + candidates[-1]["id"], endpoint, fingerprint)
                emit("restore_failed_using_cold_start")
                child = start_native(native)
        if stop.is_set():
            return
        proxy = Proxy(args.listen_port, ("127.0.0.1", upstream_port))
        threading.Thread(target=proxy.serve_forever, kwargs={"poll_interval": 0.2}, daemon=True).start()
        emit("ready", restored=restored)
        last_prune = time.monotonic()
        while not stop.wait(0.5):
            if child.poll() is not None:
                raise RuntimeError("Native runtime exited")
            if time.monotonic() - last_prune >= 3600:
                run_probe(args, "prune", endpoint, fingerprint)
                last_prune = time.monotonic()
        # Close admission and all keep-alive sockets before inspecting/saving.
        proxy.shutdown()
        proxy.close_connections()
        proxy.server_close()
        proxy = None
        try:
            slots = read_json(endpoint, "/slots", timeout=0.5)
            if len(slots) == 1 and not slots[0]["is_processing"] and slots[0].get("n_prompt_tokens", 0):
                result = run_probe(args, "save", endpoint, fingerprint, timeout=args.save_timeout)
                emit("saved", tokens=result["snapshot"]["tokens"], bytes=result["snapshot"]["bytes"])
            else:
                emit("save_skipped_busy_or_empty")
        except Exception:
            emit("save_failed_or_deadline_exceeded")
    finally:
        if proxy:
            proxy.shutdown()
            proxy.close_connections()
            proxy.server_close()
        terminate(child)
        if child is None or child.poll() is not None:
            marker.unlink(missing_ok=True)
            # A timed-out save stays pending until startup or hourly maintenance
            # confirms no writer is alive. Do not delay process teardown for I/O.


if __name__ == "__main__":
    main()
