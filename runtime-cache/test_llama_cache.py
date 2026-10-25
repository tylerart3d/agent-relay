import importlib.util
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location("llama_cache", Path(__file__).with_name("llama_cache.py"))
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class CacheWrapperTests(unittest.TestCase):
    def test_context_and_model_replacements_invalidate(self):
        with tempfile.TemporaryDirectory() as directory:
            model = Path(directory) / "model.gguf"
            model.write_bytes(b"weights-one")
            command = ["/runtime", "--model", str(model), "--ctx-size", "131072"]
            before = cache.compatibility(command, "runtime-one")
            self.assertNotEqual(before, cache.compatibility(command, "runtime-two"))
            self.assertNotEqual(before, cache.compatibility(command[:-1] + ["262144"], "runtime-one"))
            model.write_bytes(b"weights-two")
            self.assertNotEqual(before, cache.compatibility(command, "runtime-one"))

    def test_all_shards_are_fingerprinted_and_missing_shard_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "qwen-00001-of-00002.gguf"
            second = first.with_name("qwen-00002-of-00002.gguf")
            first.write_bytes(b"first")
            second.write_bytes(b"second")
            before = cache.compatibility(["/runtime", str(first)], "build")
            second.write_bytes(b"changed")
            self.assertNotEqual(before, cache.compatibility(["/runtime", str(first)], "build"))
            second.unlink()
            with self.assertRaises(FileNotFoundError):
                cache.compatibility(["/runtime", str(first)], "build")

    def test_enforces_private_single_slot_runtime(self):
        command = [str(Path(__file__).resolve()), "--host", "127.0.0.1", "--port", "{upstream_port}",
                   "--slot-save-path", "{cache_dir}", "--parallel", "1"]
        cache.validate_command(command)
        for invalid in [command + ["--parallel", "4"], command[:-1] + ["2"],
                        [x.replace("127.0.0.1", "0.0.0.0") for x in command]]:
            with self.assertRaises(ValueError):
                cache.validate_command(invalid)

    def test_streaming_bytes_and_disconnect_propagate(self):
        upstream = socket.socket()
        upstream.bind(("127.0.0.1", 0))
        upstream.listen()
        received = []
        closed = threading.Event()
        def native():
            with upstream.accept()[0] as connection:
                received.append(connection.recv(4096))
                connection.sendall(b"data: {\"token\":\"hello\"}\n\n")
                connection.settimeout(3)
                if connection.recv(1) == b"":
                    closed.set()
        thread = threading.Thread(target=native)
        thread.start()
        proxy = cache.Proxy(0, upstream.getsockname())
        serve = threading.Thread(target=proxy.serve_forever)
        serve.start()
        try:
            with socket.create_connection(proxy.server_address) as client:
                client.sendall(b"POST /v1/chat/completions HTTP/1.1\r\n\r\n")
                self.assertEqual(client.recv(4096), b"data: {\"token\":\"hello\"}\n\n")
            self.assertTrue(closed.wait(3), "consumer cancellation must close upstream")
            self.assertEqual(received, [b"POST /v1/chat/completions HTTP/1.1\r\n\r\n"])
        finally:
            proxy.shutdown()
            proxy.close_connections()
            proxy.server_close()
            upstream.close()
            thread.join(3)
            serve.join(3)


if __name__ == "__main__":
    unittest.main()
