# AdaFace worker for Bokeh

Optional, external CPU service using the official AdaFace IR18 WebFace4M checkpoint.
This worker produces 512-dimensional L2-normalized face embeddings, **not** face
detections or identity labels. Bokeh must detect landmarks and align each face to
112×112 first. Do not resize a whole photo and treat it as an aligned face.

Source: https://github.com/mk-minchul/AdaFace (retain its MIT license).
Official checkpoint: Google Drive ID `1J17_QW1Oq00EhSWObISnhWEYr2NNrg2y`,
linked from the upstream README. Model weights and dependencies are not bundled
with Agent Relay. Review upstream dataset/model terms before redistribution.

Validated source commit: `c60eaa786a42c03444f3df7096dbaf9d57ae010d`.
Checkpoint SHA-256: `7a789f6696e5abb7ac7013a8a3e272abd9bffed84e7a2ebd8145b42c9382d828`.
The checkpoint is loaded with PyTorch `weights_only=True`; no unrestricted pickle
fallback is used. Smoke validation checks real inference on synthetic crops, vector
dimensions, finite values, unit norm, and repeated-input consistency—not recognition accuracy.

Install dependencies in a dedicated Python 3.12 environment, clone the upstream
source, and download the checkpoint separately. Launch:

```text
python server.py --source /path/to/AdaFace --checkpoint /path/to/ir18.ckpt --port 5810
python -m unittest discover -s workers/adaface
```

Register as `kind: worker`, runtime `pytorch`, capability `http_service` in
llama-swap metadata. Set `checkEndpoint: /health`, `ttl: 1800`, and proxy to the
loopback worker port. The model loads on demand and is released on idle unload.
It shares the existing one-profile-per-host lifecycle; loading it can conflict
with a running LLM. Never force an unrelated active job just to get embeddings.

From Bokeh, use the local Relay endpoint:

```text
GET  http://127.0.0.1:38475/api/worker/mini/adaface-ir18/health
POST http://127.0.0.1:38475/api/worker/mini/adaface-ir18/v1/embeddings
Content-Type: application/json

{"images":["BASE64_PNG_OR_JPEG"]}
```

Response: `model`, `checkpoint_sha256`, `dimensions: 512`, `normalized: true`,
and ordered `data: [{index, embedding: [512 floats], feature_norm}]`.
Store the model/checkpoint identity with vectors; never mix embedding spaces.
`feature_norm` is an uncalibrated model output, not a confidence probability.
Maximum 16 crops and 2 MiB per request. URLs and filesystem paths are not accepted.
The service does not save crops/vectors or log request content. Bokeh owns consent,
vector storage, matching thresholds, and deletion. No photo library is scanned
by installing this worker.

Deployment validation (2026-10-19): MINI CPU runtime, two generated crops through
WORKSTATION Relay → MINI Relay → llama-swap → AdaFace: 199 ms warm round trip;
worker RSS approximately 274 MiB. Cold-start local Relay inference also passed.
These are smoke measurements, not recognition-quality or sustained-throughput benchmarks.
