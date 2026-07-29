"""Reduce a capture-out/traffic.jsonl into a doc-writable digest.

traffic.jsonl is tens of megabytes of full request/response bodies. This walks
it and emits one compact block per distinct endpoint: request headers, request
payload, and a response body with long arrays clipped to a couple of elements
and long strings truncated. Enough to write accurate documentation from,
without carrying 27 MB around.

Secrets are masked. Static assets (JS/CSS/fonts/images) are skipped -- they are
noise for API documentation.

    py tools/digest_capture.py > tools/capture-out/digest.md
"""

from __future__ import annotations

import json
import re
import sys
from collections import OrderedDict
from pathlib import Path

TRAFFIC = Path(__file__).parent / "capture-out" / "traffic.jsonl"

API_HOSTS = {
    "api.groupme.com",
    "v2.groupme.com",
    "push.groupme.com",
    "image.groupme.com",
    "cdn2.groupme.com",
}

SECRET_RE = re.compile(
    r"(token|auth|cookie|secret|signature|^sig$|password|bearer|api[-_]?key)", re.I
)
STATIC_RE = re.compile(r"\.(js|css|woff2?|png|jpe?g|gif|svg|ico|webmanifest|avatar)$", re.I)

MAX_STR = 90
MAX_ARR = 2
MAX_BODY_CHARS = 2600


def mask(k: str, v):
    if isinstance(v, str) and SECRET_RE.search(k or ""):
        return f"<{len(v)}-char secret>"
    return v


def clip(value, depth=0, key=None):
    if depth > 7:
        return "..."
    if isinstance(value, dict):
        out = OrderedDict()
        for k, v in value.items():
            if SECRET_RE.search(k):
                out[k] = f"<secret>" if isinstance(v, str) else v
            else:
                out[k] = clip(v, depth + 1, k)
        return out
    if isinstance(value, list):
        if not value:
            return []
        clipped = [clip(v, depth + 1, key) for v in value[:MAX_ARR]]
        if len(value) > MAX_ARR:
            clipped.append(f"...+{len(value) - MAX_ARR} more")
        return clipped
    if isinstance(value, str):
        v = mask(key or "", value)
        if isinstance(v, str) and len(v) > MAX_STR:
            return v[:MAX_STR] + f"…(+{len(v) - MAX_STR})"
        return v
    return value


def path_template(path: str) -> str:
    out = []
    for seg in (path or "/").split("?")[0].split("/"):
        if seg.isdigit() and len(seg) >= 4:
            out.append("{id}")
        elif re.fullmatch(r"[0-9a-f]{16,}", seg or "", re.I):
            out.append("{hash}")
        else:
            out.append(seg)
    return "/".join(out)


def pretty(raw: str | None) -> str | None:
    if not raw:
        return None
    raw = raw.strip()
    if not raw:
        return None
    try:
        parsed = json.loads(raw)
    except Exception:
        return raw[:MAX_BODY_CHARS]
    text = json.dumps(clip(parsed), indent=2, ensure_ascii=False)
    if len(text) > MAX_BODY_CHARS:
        text = text[:MAX_BODY_CHARS] + "\n…truncated"
    return text


def main() -> int:
    if not TRAFFIC.exists():
        print(f"missing {TRAFFIC}", file=sys.stderr)
        return 1

    seen: OrderedDict[tuple, dict] = OrderedDict()
    for line in TRAFFIC.open(encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            continue
        host = (r.get("host") or "").lower()
        if host not in API_HOSTS:
            continue
        path = r.get("path") or "/"
        if STATIC_RE.search(path.split("?")[0]):
            continue

        key = (r.get("method"), host, path_template(path))
        # Keep the richest example: prefer one that actually has a response body.
        prev = seen.get(key)
        if prev and len(prev.get("response_body") or "") >= len(
            r.get("response_body") or ""
        ):
            prev["_hits"] = prev.get("_hits", 1) + 1
            continue
        r["_hits"] = (prev.get("_hits", 1) + 1) if prev else 1
        seen[key] = r

    print("# Capture digest\n")
    print(f"{len(seen)} distinct API endpoints. Secrets masked, arrays clipped.\n")

    for (method, host, tmpl), r in sorted(seen.items(), key=lambda kv: (kv[0][1], kv[0][2], kv[0][0])):
        print(f"\n---\n\n## `{method} {host}{tmpl}`\n")
        print(f"- hits: {r.get('_hits')}  status: `{r.get('status')}`")
        qs = r.get("querystring")
        if qs:
            safe = []
            for pair in qs.split("&"):
                k, _, v = pair.partition("=")
                safe.append(f"{k}={'<secret>' if SECRET_RE.search(k) else v[:60]}")
            print(f"- query: `{'&'.join(safe)}`")

        req_h = r.get("request_headers") or {}
        interesting = {
            k: (f"<{len(v)}-char secret>" if SECRET_RE.search(k) else v)
            for k, v in req_h.items()
            if k.lower()
            in {
                "x-access-token",
                "x-requested-with",
                "content-type",
                "accept",
                "origin",
                "referer",
                "cookie",
                "authorization",
                "user-agent",
                "x-gm-client",
                "x-client-version",
            }
        }
        if interesting:
            print("\n**Request headers**\n")
            print("```http")
            for k, v in interesting.items():
                print(f"{k}: {v}")
            print("```")

        rb = pretty(r.get("request_body"))
        if rb:
            print("\n**Request body**\n")
            print("```json")
            print(rb)
            print("```")

        resp_h = r.get("response_headers") or {}
        ct = resp_h.get("Content-Type") or resp_h.get("content-type")
        if ct:
            print(f"\n- response content-type: `{ct}`")

        sb = pretty(r.get("response_body"))
        if sb:
            print("\n**Response**\n")
            print("```json")
            print(sb)
            print("```")

    return 0


if __name__ == "__main__":
    sys.exit(main())
