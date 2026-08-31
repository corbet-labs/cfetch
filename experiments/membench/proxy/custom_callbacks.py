# One JSONL line per proxied request: the counting ruler for every arm.
# Wired via litellm-config.yaml -> general_settings.callbacks.
import json
import os
import time

LOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results", "usage.jsonl")


def _write(row):
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps(row) + "\n")


def my_logger(kwargs, response, start_time, end_time):
    try:
        usage = getattr(response, "usage", None) or {}
        _write({
            "ts": time.time(),
            "model": kwargs.get("model", ""),
            "input_tokens": usage.get("prompt_tokens", 0),
            "output_tokens": usage.get("completion_tokens", 0),
            "cache_read": usage.get("cache_read_input_tokens", 0),
            "cache_write": usage.get("cache_creation_input_tokens", 0),
        })
    except Exception as e:  # a broken logger must never break the proxy
        print("usage-logger error:", e)


async def async_pre_call_hook(self, user_api_key_dict, cache, request, call_type):
    return request
