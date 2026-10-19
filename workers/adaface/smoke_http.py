"""Exercise a running Relay worker route using generated crops only."""
import argparse
import json
import math
import urllib.request
from test_server import image_data

parser = argparse.ArgumentParser()
parser.add_argument("--url", default="http://127.0.0.1:38475/api/worker/mini/adaface-ir18")
args = parser.parse_args()
with urllib.request.urlopen(args.url + "/health", timeout=120) as response:
    assert json.load(response)["status"] == "ready"
request = urllib.request.Request(args.url + "/v1/embeddings",
    data=json.dumps({"images": [image_data(), image_data()]}).encode(),
    headers={"Content-Type": "application/json"})
with urllib.request.urlopen(request, timeout=120) as response:
    result = json.load(response)
assert result["dimensions"] == 512
assert len(result["data"]) == 2
for item in result["data"]:
    assert len(item["embedding"]) == 512
    assert all(math.isfinite(v) for v in item["embedding"])
    assert abs(sum(v*v for v in item["embedding"]) - 1) < 1e-5
print(json.dumps({"ok": True, "model": result["model"], "dimensions": 512,
                  "batch": 2, "checkpoint_sha256": result["checkpoint_sha256"]}))
