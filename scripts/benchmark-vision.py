"""Bounded synthetic-page throughput benchmark through an OpenAI streaming proxy."""
import argparse
import base64
import concurrent.futures
import json
import pathlib
import threading
import time
import urllib.request

p = argparse.ArgumentParser()
p.add_argument('--image', required=True)
p.add_argument('--output', required=True)
p.add_argument('--label', required=True)
p.add_argument('--concurrency', default='1,2,3,4')
p.add_argument('--max-tokens', type=int, default=1500)
p.add_argument('--model', required=True)
p.add_argument('--url', default='http://127.0.0.1:38475/v1/chat/completions')
a = p.parse_args()
image = 'data:image/png;base64,' + base64.b64encode(pathlib.Path(a.image).read_bytes()).decode()

def run(index, barrier):
    payload = {'model': a.model, 'messages': [{'role': 'user', 'content': [
        {'type': 'text', 'text': f'Transcription job {index}. Transcribe every line of this page exactly, in reading order. Return only the full transcription, no commentary.'},
        {'type': 'image_url', 'image_url': {'url': image}}
    ]}], 'max_tokens': a.max_tokens, 'temperature': 0, 'seed': 42,
        'chat_template_kwargs': {'enable_thinking': False}, 'thinking_budget_tokens': 0,
        'stream': True, 'stream_options': {'include_usage': True}, 'cache_prompt': False}
    request = urllib.request.Request(a.url,
        data=json.dumps(payload).encode(), headers={'Content-Type': 'application/json'})
    barrier.wait()
    start = time.monotonic()
    first = None
    usage, timings, answer, reasoning, finish = {}, {}, '', '', None
    try:
        with urllib.request.urlopen(request, timeout=900) as response:
            status = response.status
            for line in response:
                if not line.startswith(b'data: '):
                    continue
                data = line[6:].strip()
                if data == b'[DONE]':
                    break
                event = json.loads(data)
                if event.get('error'):
                    raise RuntimeError('Upstream returned a streaming error')
                if event.get('usage'):
                    usage = event['usage']
                if event.get('timings'):
                    timings = event['timings']
                for choice in event.get('choices', []):
                    delta = choice.get('delta', {})
                    text = delta.get('content') or ''
                    if text and first is None:
                        first = time.monotonic() - start
                    answer += text
                    reasoning += delta.get('reasoning_content') or ''
                    finish = choice.get('finish_reason') or finish
        if not finish or not usage.get('completion_tokens') or not answer:
            raise RuntimeError('Incomplete stream: missing finish, usage or answer')
        return {'index': index, 'status': status, 'seconds': time.monotonic()-start,
            'ttft_seconds': first, 'usage': usage, 'timings': timings,
            'finish_reason': finish, 'answer_chars': len(answer), 'reasoning_chars': len(reasoning)}
    except Exception as error:
        return {'index': index, 'seconds': time.monotonic()-start, 'error': str(error)}

for count in map(int, a.concurrency.split(',')):
    barrier = threading.Barrier(count)
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=count) as pool:
        results = list(pool.map(lambda i: run(i, barrier), range(count)))
    elapsed = time.monotonic()-start
    tokens = sum(r.get('usage', {}).get('completion_tokens', 0) for r in results)
    result = {'label': a.label, 'concurrency': count, 'wall_seconds': elapsed,
        'total_output_tokens': tokens, 'aggregate_tokens_per_second': tokens/elapsed,
        'mean_request_seconds': sum(r['seconds'] for r in results)/count, 'requests': results}
    with open(a.output, 'a', encoding='utf-8') as f:
        f.write(json.dumps(result)+'\n')
    print(json.dumps(result), flush=True)
    if any('error' in item for item in results):
        raise SystemExit('Benchmark failed; inspect the recorded request errors')
