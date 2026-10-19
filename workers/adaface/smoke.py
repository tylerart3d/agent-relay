"""Validate the real checkpoint with a synthetic crop; no personal images."""
import argparse
import json
import math
from pathlib import Path
from server import Encoder
from PIL import Image

parser = argparse.ArgumentParser()
parser.add_argument("--source", required=True)
parser.add_argument("--checkpoint", required=True)
args = parser.parse_args()
model = Encoder(args.source, args.checkpoint, 2)
result = model.encode([Image.new("RGB", (112, 112), (70, 140, 210))] * 2)
vectors = [item["embedding"] for item in result["data"]]
assert len(vectors) == 2 and all(len(v) == 512 for v in vectors)
assert all(math.isfinite(x) for v in vectors for x in v)
assert all(abs(sum(x*x for x in v) - 1) < 1e-5 for v in vectors)
assert max(abs(a-b) for a,b in zip(*vectors)) < 1e-5
print(json.dumps({"ok": True, "dimensions": 512, "normalized": True,
                  "checkpoint_sha256": model.digest}, sort_keys=True))
