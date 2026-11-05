# CPU document embeddings

Agent Relay 0.25.2 supports embedding-only text profiles in the `cpu` resource
pool alongside chat/vision. Merge the model and isolated persistent group in
[`llama-swap-embeddings.yaml`](../examples/llama-swap-embeddings.yaml).
Both metadata and group configuration are required. `--device none
--no-op-offload` keeps inference on CPU; metadata alone does not set placement.

Embedding startup/reload preserves chat requests. Normal chat switches preserve
CPU embeddings. Explicit forced chat switches and host-wide unload/stop still
affect all models. The tray prioritizes chat as the host's loaded model;
llama-swap `/running` lists concurrent processes. General resource-pool
scheduling is not implemented.

Send `POST http://127.0.0.1:38475/v1/embeddings`:

```json
{
  "model": "workstation/qwen3-embedding-0.6b-q8",
  "input": ["A document passage to index."],
  "encoding_format": "float"
}
```

Loopback requires no API key. From another Relay machine use its local proxy
with the same qualified model ID. Requests traverse llama-swap so idle TTL
tracks real activity. After 1,800 idle seconds embeddings unload; the next
request reloads them. Embeddings stay out of harness chat catalogs and do not
change harness routes.

The official Qwen3-Embedding-0.6B Q8 GGUF returns 1,024-dimensional normalized
vectors using last-token pooling. This profile caps individual inputs at
8,192 tokens; the model supports 32K. Prefer paragraph-based chunks around
300–800 tokens for retrieval. Extraction/OCR and database writes are separate.

Embed passages as plain text. For queries use the recommended instruction format:
`Instruct: Given a search query, retrieve relevant document passages\nQuery: Where is the warranty information?`.
Keep model, quantization, dimensions, pooling and query convention consistent;
store the embedding identity with vectors and re-embed when it changes.

Source: [official model card](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF).
