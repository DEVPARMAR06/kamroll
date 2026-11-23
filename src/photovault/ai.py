"""Optional smart tags through a local Ollama. Nothing here runs unless the person turns it on.

Two jobs: suggest a title for a trip (any chat model), and describe a picture in a few words (needs a
model that can see images). Everything goes to this computer only; other addresses are refused.
"""
from __future__ import annotations

import base64
import json
import re
import urllib.error
import urllib.request
from urllib.parse import urlparse

KINDS = ("memory", "screenshot", "social", "document", "junk")


class AiError(Exception):
    """A readable reason the model could not help. The app shows it as is."""


def is_loopback(host: str) -> bool:
    u = urlparse(host)
    return u.scheme in ("http", "https") and u.hostname in ("127.0.0.1", "localhost", "::1")


def _post(host: str, path: str, payload: dict, timeout: float) -> dict:
    if not is_loopback(host):
        raise AiError("Only a local Ollama (127.0.0.1 or localhost) is allowed.")
    req = urllib.request.Request(host.rstrip("/") + path, data=json.dumps(payload).encode("utf-8"),
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:  # noqa: S310 - loopback only, checked above
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        try:
            detail = json.loads(e.read().decode("utf-8")).get("error", "")
        except Exception:  # noqa: BLE001
            detail = ""
        raise AiError(detail or f"Ollama answered with an error ({e.code}).") from e
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise AiError("Ollama did not answer. Check that it is running.") from e


def parse_json(text: str) -> dict:
    """Models sometimes wrap the JSON in words. Take the first object that parses."""
    try:
        out = json.loads(text)
        if isinstance(out, dict):
            return out
    except ValueError:
        pass
    for m in re.finditer(r"\{.*?\}", text, re.S):
        try:
            out = json.loads(m.group(0))
            if isinstance(out, dict):
                return out
        except ValueError:
            continue
    raise AiError("The model did not return a usable answer.")


def chat_json(host: str, model: str, prompt: str, images: list[bytes] | None = None, timeout: float = 180) -> dict:
    msg: dict = {"role": "user", "content": prompt}
    if images:
        msg["images"] = [base64.b64encode(b).decode("ascii") for b in images]
    out = _post(host, "/api/chat", {"model": model, "messages": [msg], "stream": False, "format": "json",
                                    "options": {"temperature": 0.2}}, timeout)
    return parse_json((out.get("message") or {}).get("content", ""))


def _clean(text, limit: int, trailing: str = ".") -> str:
    out = re.sub(r"\s+", " ", str(text or "")).strip(" \"'")
    return out.rstrip(trailing)[:limit]


def suggest_trip(host: str, model: str, *, places: list[str], start: str, end: str, days: int,
                 files: int, mix: dict[str, int]) -> tuple[str, str]:
    """(title, summary) for a trip, from facts the app already knows."""
    kinds = ", ".join(f"{n} {k}" for k, n in mix.items() if n)
    prompt = (
        "You name photo albums for a person's trips. Reply with JSON only: "
        '{"title": "a warm title of two to five words", "summary": "one friendly sentence"}. '
        "Do not invent facts. Use only what is given.\n"
        f"Places visited: {', '.join(places) or 'unknown'}.\nDates: {start} to {end} ({days} day{'s' if days != 1 else ''}).\n"
        f"Files: {files} ({kinds or 'photos and videos'}).")
    out = chat_json(host, model, prompt)
    title, summary = _clean(out.get("title"), 60), _clean(out.get("summary"), 220, trailing="")
    if not title:
        raise AiError("The model did not suggest a title.")
    return title, summary


VISION_PROMPT = (
    "Look at this picture. Reply with JSON only: "
    '{"tags": ["up to five short lowercase words for what it shows, such as beach, sunset, food, friends"], '
    '"caption": "one short sentence", '
    '"kind": "one of memory, screenshot, social, document, junk"}. '
    "Use document for receipts, forms, whiteboards and printed pages. Use junk for blurry, dark or accidental shots.")


def describe_picture(host: str, model: str, jpeg: bytes) -> dict:
    """{'tags': [...], 'caption': str, 'kind': str|None} for one picture. Needs a model that can see images."""
    out = chat_json(host, model, VISION_PROMPT, images=[jpeg])
    raw = out.get("tags") or []
    tags = []
    for t in raw if isinstance(raw, list) else str(raw).split(","):
        t = _clean(t, 24).lower()
        if t and t not in tags:
            tags.append(t)
    kind = _clean(out.get("kind"), 20).lower()
    return {"tags": tags[:5], "caption": _clean(out.get("caption"), 160, trailing=""), "kind": kind if kind in KINDS else None}
