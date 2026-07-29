"""Pull every push.groupme.com/faye exchange out of a capture, in order.

The Bayeux protocol multiplexes handshake, subscribe, and connect over a single
path, so the per-endpoint digest collapses them into one row. Realtime needs
each frame in sequence, so this dumps them individually with the `message`
query parameter URL-decoded.

Also reports whether the session upgraded to a websocket, because frames sent
after a 101 are invisible to an HTTP-level proxy and would have to be captured
another way.

    py tools/extract_faye.py
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from urllib.parse import parse_qs, unquote

TRAFFIC = Path(__file__).parent / "capture-out" / "traffic.jsonl"
SECRET_RE = re.compile(r"(token|auth|cookie|secret|signature|password)", re.I)


def scrub(obj):
    """Blank credential-ish values but keep channel/id/subscription structure."""
    if isinstance(obj, dict):
        return {
            k: ("<secret>" if SECRET_RE.search(k) and isinstance(v, str) else scrub(v))
            for k, v in obj.items()
        }
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    if isinstance(obj, str) and len(obj) > 300:
        return obj[:300] + f"…(+{len(obj) - 300})"
    return obj


# Faye posts `message=<urlencoded json>` as a form body, so the access token
# arrives nested at ext.access_token inside a percent-encoded string. Scrubbing
# only parsed JSON dicts misses it entirely and prints a live credential.
TOKEN_IN_TEXT_RE = re.compile(
    r'("(?:access_token|token|password|signature)"\s*:\s*")([^"]+)(")'
)


def scrub_text(raw: str) -> str:
    return TOKEN_IN_TEXT_RE.sub(r"\1<secret>\3", raw)


def show(label: str, raw: str | None):
    if not raw:
        return
    raw = raw.strip()
    if not raw:
        return

    # Form-encoded Faye envelope: decode so the JSON inside is readable, and so
    # the scrubber can actually see the keys it needs to mask.
    if raw.startswith("message="):
        raw = unquote(raw[len("message=") :])

    try:
        parsed = json.loads(raw)
        print(f"  {label}:")
        print(
            "    "
            + json.dumps(scrub(parsed), indent=2, ensure_ascii=False).replace(
                "\n", "\n    "
            )
        )
    except Exception:
        # Last line of defence: mask on the raw text before anything is printed.
        print(f"  {label}: {scrub_text(raw)[:600]}")


def main() -> int:
    if not TRAFFIC.exists():
        print(f"missing {TRAFFIC}")
        return 1

    n = 0
    upgraded = False
    for line in TRAFFIC.open(encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            continue
        if "push.groupme.com" not in (r.get("host") or ""):
            continue

        n += 1
        status = r.get("status")
        if status == 101:
            upgraded = True
        print(f"\n=== #{n}  {r.get('method')} {r.get('path','').split('?')[0]}  -> {status} ===")

        qs = r.get("querystring") or ""
        params = parse_qs(qs)
        for key, values in params.items():
            if key == "message":
                for v in values:
                    show("request message", unquote(v))
            elif key not in ("jsonp",):
                print(f"  {key}: {values[0][:120]}")

        show("request body", r.get("request_body"))

        body = r.get("response_body")
        if body:
            # JSONP wrapper: /**/__jsonpN__([...]);
            m = re.search(r"__jsonp\d+__\((.*)\);?\s*$", body.strip(), re.S)
            show("response", m.group(1) if m else body)

    print(f"\n---\n{n} faye exchanges captured.")
    if upgraded:
        print(
            "A 101 upgrade was observed. Frames exchanged after the upgrade travel\n"
            "inside the websocket and are NOT visible to an HTTP-level proxy, so any\n"
            "subscribe/publish that happened post-upgrade is not in this capture."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
