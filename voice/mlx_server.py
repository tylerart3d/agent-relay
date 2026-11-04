"""Supervised, single-model MLX VoiceChat server; not a general MLX model loader.

Install the pinned mlx-vlm dependency in requirements-mlx.txt on Apple silicon.
llama-swap starts/stops this foreground process and owns the resident weights.
"""
from __future__ import annotations

import argparse
import asyncio
from contextlib import asynccontextmanager
from pathlib import Path


def fixed_model_loader(model_path: str, load):
    """Clients may only open the configured model through the public alias."""
    def loader(name: str):
        if name != "agentrelay":
            raise ValueError("This voice runtime only serves model 'agentrelay'")
        return load(model_path)
    return loader


def create_app(model_path: str):
    # Lazy imports keep argument validation/help and unit tests usable off-Mac.
    import numpy as np
    from fastapi import FastAPI
    from mlx_vlm.server.realtime import (
        RealtimeVoiceChatEngine, load_realtime_voicechat, register_routes,
    )
    from mlx_vlm.server.runtime import runtime

    @asynccontextmanager
    async def lifespan(app):
        engine = RealtimeVoiceChatEngine(loader=fixed_model_loader(model_path, load_realtime_voicechat))
        runtime.realtime_engine = engine
        try:
            # Load and compile on the SAME worker used by inference. Health only
            # becomes available after a real silent frame, not after importing MLX.
            await asyncio.to_thread(engine.open, "warmup", model="agentrelay", seed=0)
            await asyncio.to_thread(engine.push, "warmup", np.zeros(1280, dtype=np.float32), 16000)
            await asyncio.to_thread(engine.close, "warmup")
            yield
        finally:
            await asyncio.to_thread(engine.stop_and_join)
            runtime.realtime_engine = None

    app = FastAPI(lifespan=lifespan, docs_url=None, redoc_url=None, openapi_url=None)

    @app.get("/v1/realtime/health")
    async def health():
        return {"status": "ok", "service": "agent-relay-mlx-voicechat", "protocol": "mlx_voicechat"}

    register_routes(app)
    return app


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path, help="Downloaded MLX VoiceChat model directory")
    parser.add_argument("--port", type=int, default=9000)
    args = parser.parse_args()
    model = args.model.expanduser().resolve()
    if not (model / "config.json").is_file():
        parser.error("--model must be a downloaded model directory containing config.json")
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    import uvicorn
    # No per-request model downloads, no remote code, no request-content logging.
    uvicorn.run(create_app(str(model)), host="127.0.0.1", port=args.port,
                ws_max_size=262144, ws_max_queue=4, access_log=False)


if __name__ == "__main__":
    main()
