"""Recording mock LLM upstream for the differential oracle.
Usage: python3 mock_upstream.py PORT
Behavior is chosen by the requested model id:
  *error500* / *error429* / *error400* / *error401* -> that HTTP status,
  *badjson* -> 200 with a non-JSON body, *tool* -> a tool call,
  *slow* -> chat stream with 2s pauses between chunks (stall detection),
  otherwise a plain text answer. Streaming follows the request's `stream`.
  POST */api/chat answers in native Ollama format (NDJSON when streaming).
GET /__log returns and clears the recorded requests."""
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = []
LOCK = threading.Lock()
SKIP_HEADERS = {"host", "content-length", "connection", "accept-encoding"}

def chat_body(model, tool=False):
    msg = {"role": "assistant", "content": "Hello from mock"}
    finish = "stop"
    if tool:
        msg = {"role": "assistant", "content": None, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}
        finish = "tool_calls"
    return {"id": "chatcmpl-mock", "object": "chat.completion", "created": 1700000000, "model": model,
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}}

def chat_stream(model, tool=False):
    base = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 1700000000, "model": model}
    if tool:
        yield {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": None, "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}]}, "finish_reason": None}]}
        yield {**base, "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": None}]}
        yield {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}
    else:
        yield {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}]}
        yield {**base, "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": None}]}
        yield {**base, "choices": [{"index": 0, "delta": {"content": " from mock"}, "finish_reason": None}]}
        yield {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
    yield {**base, "choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}}

def messages_body(model, tool=False):
    content = [{"type": "text", "text": "Hello from mock"}]
    stop = "end_turn"
    if tool:
        content = [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}]
        stop = "tool_use"
    return {"id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": content,
            "stop_reason": stop, "stop_sequence": None, "usage": {"input_tokens": 11, "output_tokens": 7}}

def messages_stream(model, tool=False):
    yield ("message_start", {"type": "message_start", "message": {"id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 11, "output_tokens": 1}}})
    if tool:
        yield ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}})
        yield ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"Paris\"}"}})
    else:
        yield ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        yield ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}})
        yield ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " from mock"}})
    yield ("content_block_stop", {"type": "content_block_stop", "index": 0})
    yield ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if tool else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 7}})
    yield ("message_stop", {"type": "message_stop"})

def responses_body(model, tool=False):
    if tool:
        output = [{"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"}]
    else:
        output = [{"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "Hello from mock", "annotations": []}]}]
    return {"id": "resp_mock", "object": "response", "created_at": 1700000000, "status": "completed", "model": model, "output": output,
            "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18}}

def gemini_body(model, tool=False):
    part = {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}} if tool else {"text": "Hello from mock"}
    return {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 7, "totalTokenCount": 18}, "modelVersion": model}

def parse_multipart(raw, content_type):
    """Parts of a multipart body as [{name, filename?, content_type?, value | size+sha}]."""
    import hashlib, re as _re
    m = _re.search(r'boundary="?([^";]+)"?', content_type or "")
    if not m:
        return None
    boundary = ("--" + m.group(1)).encode()
    parts = []
    for chunk in raw.split(boundary)[1:]:
        if chunk.startswith(b"--"):
            break
        chunk = chunk[2:] if chunk.startswith(b"\r\n") else chunk
        head, _, data = chunk.partition(b"\r\n\r\n")
        data = data[:-2] if data.endswith(b"\r\n") else data
        info = {}
        for line in head.decode(errors="replace").split("\r\n"):
            k, _, v = line.partition(":")
            if k.lower() == "content-disposition":
                for attr in v.split(";")[1:]:
                    ak, _, av = attr.strip().partition("=")
                    info[ak] = av.strip('"')
            elif k.lower() == "content-type":
                info["content_type"] = v.strip()
        if "filename" in info:
            info["size"] = len(data)
            info["sha"] = hashlib.sha256(data).hexdigest()[:16]
        else:
            info["value"] = data.decode(errors="replace")
        parts.append(info)
    return parts

def media_image_body(model):
    if "emptyimg" in model:
        return {"created": 1700000000, "data": []}
    if "urlimg" in model:
        return {"created": 1700000000, "data": [{"url": "http://127.0.0.1:9/x.png"}]}
    return {"created": 1700000000, "data": [{"b64_json": "aGVsbG8=", "revised_prompt": "rp"}],
            "usage": {"input_tokens": 5, "output_tokens": 9, "total_tokens": 14}}

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args): pass

    def reply(self, status, body, content_type="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        if getattr(self, "echo_name", None): data = data.replace(b"get_weather", self.echo_name.encode())
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def stream(self, events):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        # Echo-tool streams use compact JSON, like real providers.
        dumps = (lambda v: json.dumps(v, separators=(",", ":"))) if getattr(self, "echo_name", None) else json.dumps
        for event in events:
            if isinstance(event, tuple):
                chunk = f"event: {event[0]}\ndata: {dumps(event[1])}\n\n"
            elif event == "[DONE]":
                chunk = "data: [DONE]\n\n"
            else:
                chunk = f"data: {dumps(event)}\n\n"
            data = chunk.encode()
            if getattr(self, "echo_name", None): data = data.replace(b"get_weather", self.echo_name.encode())
            self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")

    def record(self, body):
        headers = {k.lower(): v for k, v in self.headers.items() if k.lower() not in SKIP_HEADERS}
        if headers.get("content-type", "").startswith("multipart/"):
            headers["content-type"] = "multipart/form-data; boundary=<boundary>"
        with LOCK:
            LOG.append({"method": self.command, "path": self.path, "headers": headers, "body": body})

    def do_GET(self):
        if self.path == "/__log":
            with LOCK:
                out = list(LOG); LOG.clear()
            return self.reply(200, out)
        self.record(None)
        if self.path.split("?")[0].endswith("/models"):
            return self.reply(200, {"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        self.reply(404, {"error": {"message": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        multipart = None
        if (self.headers.get("content-type") or "").startswith("multipart/"):
            multipart = parse_multipart(raw, self.headers.get("content-type"))
            body = {"__multipart": multipart}
        else:
            try:
                body = json.loads(raw) if raw else None
            except Exception:
                body = {"__raw": raw.decode(errors="replace")[:2000]}
        self.record(body)
        model = body.get("model", "") if isinstance(body, dict) else ""
        fields = {}
        if multipart is not None:
            fields = {p["name"]: p.get("value", "") for p in multipart if "value" in p}
            model = fields.get("model", "")
        if "/models/" in self.path and ":" in self.path:
            model = self.path.split("/models/")[1].split(":")[0]
        tool = "tool" in model
        # *echotool*: the tool call names the request's last declared tool.
        self.echo_name = None
        if "echotool" in model and isinstance(body, dict) and isinstance(body.get("tools"), list) and body["tools"]:
            last = body["tools"][-1]
            self.echo_name = last.get("name") or (last.get("function") or {}).get("name")
        for code in (500, 429, 400, 401):
            if f"error{code}" in model:
                return self.reply(code, {"error": {"message": f"mock error {code}", "type": "mock_error"}})
        if "badjson" in model:
            return self.reply(200, b"not json at all")
        stream = isinstance(body, dict) and body.get("stream") is True
        path = self.path.split("?")[0]
        if path.endswith("/api/chat"):
            msg = {"role": "assistant", "content": "Hello from mock"}
            if tool:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "get_weather", "arguments": {"city": "Paris"}}}]}
            final = {"model": model, "created_at": "2023-08-04T08:52:19.385406455Z", "message": {"role": "assistant", "content": ""}, "done_reason": "stop", "done": True, "prompt_eval_count": 11, "eval_count": 7}
            if not stream:
                return self.reply(200, {**final, "message": msg})
            lines = [{"model": model, "created_at": "2023-08-04T08:52:19.385406455Z", "message": {"role": "assistant", "content": "Hello"}, "done": False},
                     {"model": model, "created_at": "2023-08-04T08:52:19.385406455Z", "message": {"role": "assistant", "content": " from mock"}, "done": False}, final]
            data = "".join(json.dumps(l) + "\n" for l in lines).encode()
            return self.reply(200, data, "application/x-ndjson")
        if path.endswith("/chat/completions") and stream and "slow" in model:
            import time
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            for i, event in enumerate(list(chat_stream(model)) + ["[DONE]"]):
                if i >= 2:
                    time.sleep(2)
                chunk = ("data: [DONE]\n\n" if event == "[DONE]" else f"data: {json.dumps(event)}\n\n").encode()
                try:
                    self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n"); self.wfile.flush()
                except Exception:
                    return
            self.wfile.write(b"0\r\n\r\n")
            return
        if path.endswith("/chat/completions"):
            return self.stream(list(chat_stream(model, tool)) + ["[DONE]"]) if stream else self.reply(200, chat_body(model, tool))
        if path.endswith("/messages"):
            return self.stream(list(messages_stream(model, tool))) if stream else self.reply(200, messages_body(model, tool))
        if path.endswith("/responses"):
            if stream:
                resp = responses_body(model, tool)
                events = [("response.created", {"type": "response.created", "sequence_number": 0, "response": {**resp, "status": "in_progress", "output": []}}),
                          ("response.output_text.delta", {"type": "response.output_text.delta", "sequence_number": 1, "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "Hello from mock"}),
                          ("response.completed", {"type": "response.completed", "sequence_number": 2, "response": resp})]
                return self.stream(events)
            return self.reply(200, responses_body(model, tool))
        if ":embedContent" in path or ":batchEmbedContents" in path:
            n = len((body or {}).get("requests", [])) if ":batch" in path else 0
            if n:
                return self.reply(200, {"embeddings": [{"values": [0.1, 0.2]} for _ in range(n)], "usageMetadata": {"promptTokenCount": 4, "totalTokenCount": 4}})
            return self.reply(200, {"embedding": {"values": [0.1, 0.2]}, "usageMetadata": {"promptTokenCount": 2, "totalTokenCount": 2}})
        if ":generateContent" in path and "image" in model:
            return self.reply(200, {"candidates": [{"content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}]}}],
                                    "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}})
        if path.endswith("/images/generations") or path.endswith("/images/edits") or path.endswith("/images"):
            return self.reply(200, media_image_body(model))
        if path.endswith("/audio/speech"):
            if "text/event-stream" in (self.headers.get("accept") or ""):
                return self.stream([{"type": "speech.audio.delta", "audio": "AAEC"}, {"type": "speech.audio.done", "usage": {"input_tokens": 3, "output_tokens": 5, "total_tokens": 8}}])
            return self.reply(200, b"\x00\x01AUDIO\xff", "audio/mpeg")
        if path.endswith("/audio/transcriptions"):
            fmt = fields.get("response_format", "json")
            if fmt == "text":
                return self.reply(200, b"hello transcript", "text/plain")
            out = {"text": "hello transcript", "usage": {"type": "tokens", "input_tokens": 12, "output_tokens": 3, "total_tokens": 15}}
            if fmt == "verbose_json":
                out.update({"language": "english", "duration": 1.5, "segments": [{"id": 0, "text": "hello transcript"}]})
            return self.reply(200, out)
        if ":streamGenerateContent" in path:
            return self.stream([gemini_body(model, tool)])
        if ":generateContent" in path:
            return self.reply(200, gemini_body(model, tool))
        if path.endswith("/completions"):
            if stream:
                base = {"id": "cmpl-mock", "object": "text_completion", "created": 1700000000, "model": model}
                return self.stream([{**base, "choices": [{"text": "Hello", "index": 0, "logprobs": None, "finish_reason": None}]},
                                    {**base, "choices": [{"text": " from mock", "index": 0, "logprobs": None, "finish_reason": "stop"}]},
                                    {**base, "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}}, "[DONE]"])
            return self.reply(200, {"id": "cmpl-mock", "object": "text_completion", "created": 1700000000, "model": model,
                                    "choices": [{"text": "Hello from mock", "index": 0, "logprobs": None, "finish_reason": "stop"}],
                                    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}})
        if path.endswith("/embeddings"):
            return self.reply(200, {"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}], "model": model, "usage": {"prompt_tokens": 3, "total_tokens": 3}})
        if path.endswith("/systemone"):
            qs = (body or {}).get("questions", {})
            return self.reply(200, {"model": model, "answers": {k: {"type": "noul", "noul": 0.5} for k in qs}, "usage": {"input_tokens": 1, "output_tokens": 1}})
        self.reply(404, {"error": {"message": "unknown path " + path}})

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
