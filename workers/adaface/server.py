"""Private, stateless face-crop embedding worker for Bokeh. No image persistence."""
import argparse
import base64
import binascii
import hashlib
import importlib.util
import io
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

MAX_BODY = 2 * 1024 * 1024
MAX_BATCH = 16


def decode_crops(payload):
    from PIL import Image
    if not isinstance(payload, dict) or set(payload) != {"images"}:
        raise ValueError("provide only images: a list of base64 encoded aligned 112x112 PNG/JPEG crops")
    images = payload["images"]
    if not isinstance(images, list) or not 1 <= len(images) <= MAX_BATCH:
        raise ValueError("images must contain 1 to 16 crops")
    result = []
    for encoded in images:
        if not isinstance(encoded, str) or len(encoded) > 128 * 1024:
            raise ValueError("each crop must be a base64 string under 128 KiB")
        try:
            raw = base64.b64decode(encoded, validate=True)
            with Image.open(io.BytesIO(raw)) as image:
                if image.size != (112, 112) or image.format not in ("PNG", "JPEG"):
                    raise ValueError("crops must be aligned 112x112 PNG or JPEG; no automatic resizing")
                result.append(image.convert("RGB"))
        except (binascii.Error, OSError) as error:
            raise ValueError("invalid base64 image") from error
    return result


class Encoder:
    def __init__(self, source, checkpoint, threads):
        import torch
        torch.set_num_threads(threads)
        spec = importlib.util.spec_from_file_location("adaface_net", Path(source) / "net.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.model = module.build_model("ir_18")
        state = torch.load(checkpoint, map_location="cpu", weights_only=True)["state_dict"]
        self.model.load_state_dict({key[6:]: val for key, val in state.items() if key.startswith("model.")})
        self.model.eval()
        self.digest = hashlib.sha256(Path(checkpoint).read_bytes()).hexdigest()

    def encode(self, images):
        import numpy as np
        import torch
        # The official AdaFace checkpoint expects BGR, not RGB, normalized to [-1,1].
        batch = np.stack([np.asarray(image, dtype=np.float32)[:, :, ::-1].transpose(2, 0, 1) for image in images])
        tensor = torch.from_numpy(np.ascontiguousarray(((batch / 255.0) - 0.5) / 0.5))
        with torch.inference_mode():
            features, norms = self.model(tensor)
            features = torch.nn.functional.normalize(features, dim=1)
        return {"model": "adaface-ir18-webface4m", "checkpoint_sha256": self.digest,
                "dimensions": 512, "normalized": True,
                "data": [{"index": i, "embedding": feature.tolist(), "feature_norm": float(norms[i].item())}
                         for i, feature in enumerate(features)]}


def make_handler(encoder):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass  # Never log image bodies, biometric vectors, or user identifiers.

        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def reply(self, code, body):
            data = json.dumps(body, allow_nan=False).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path == "/v1/models":
                # Relay's lifecycle readiness probe; this does not expose a chat API.
                self.reply(200, {"object": "list", "data": [{"id": "adaface-ir18", "object": "model"}]})
            elif self.path in ("/health", "/v1/info"):
                self.reply(200, {"status": "ready", "model": "adaface-ir18-webface4m", "dimensions": 512,
                                 "input": "aligned 112x112 RGB PNG/JPEG crops", "max_batch": MAX_BATCH,
                                 "checkpoint_sha256": encoder.digest})
            else:
                self.reply(404, {"error": "unknown endpoint"})

        def do_POST(self):
            if self.path != "/v1/embeddings":
                self.reply(404, {"error": "unknown endpoint"})
                return
            try:
                if self.headers.get("Transfer-Encoding"):
                    raise ValueError("Content-Length required")
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= MAX_BODY:
                    self.reply(413, {"error": "body must be 1 byte to 2 MiB"})
                    return
                payload = json.loads(self.rfile.read(size))
                images = decode_crops(payload)
            except (ValueError, TypeError, TimeoutError):
                self.reply(400, {"error": "invalid request: send 1-16 aligned 112x112 PNG/JPEG crops in images"})
                return
            try:
                self.reply(200, encoder.encode(images))
            except Exception:
                self.reply(500, {"error": "embedding computation failed"})
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True)
    parser.add_argument("--checkpoint", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--threads", type=int, default=2)
    args = parser.parse_args()
    encoder = Encoder(args.source, args.checkpoint, max(1, args.threads))
    # Serial requests bound peak memory and CPU; Relay provides network access.
    HTTPServer(("127.0.0.1", args.port), make_handler(encoder)).serve_forever()


if __name__ == "__main__":
    main()
