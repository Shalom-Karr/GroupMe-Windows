"""Summarise captured Faye websocket frames.

The whole realtime protocol is multiplexed over one socket, so this groups the
frames by Bayeux channel and shows one representative of each shape -- which is
what implementing a subscriber actually requires.

Credentials are masked. Message text is truncated, since the point is the
envelope, not the content.

    py tools/analyze_ws.py
"""

from __future__ import annotations

import json
import re
from collections import Counter, OrderedDict
from pathlib import Path

WS = Path(__file__).parent / "capture-out" / "websocket.jsonl"
SECRET_RE = re.compile(r"(token|auth|secret|password|signature)", re.I)


def scrub(v, key=None, depth=0):
    if depth > 8:
        return "..."
    if isinstance(v, dict):
        return {k: scrub(x, k, depth + 1) for k, x in v.items()}
    if isinstance(v, list):
        return [scrub(x, key, depth + 1) for x in v[:3]] + (
            [f"...+{len(v) - 3}"] if len(v) > 3 else []
        )
    if isinstance(v, str):
        if key and SECRET_RE.search(key):
            return f"<{len(v)}-char secret>"
        return v if len(v) <= 70 else v[:70] + f"…(+{len(v) - 70})"
    return v


def main() -> int:
    if not WS.exists():
        print(f"missing {WS}")
        return 1

    frames = []
    for line in WS.open(encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            frames.append(json.loads(line))
        except Exception:
            continue

    print(f"{len(frames)} frames\n")

    directions = Counter(f.get("method", "?").replace("Network.webSocket", "") for f in frames)
    for d, n in directions.most_common():
        print(f"  {d:<28} {n}")
    print()

    urls = {f.get("url") for f in frames if f.get("url")}
    for u in sorted(x for x in urls if x):
        print(f"  socket: {u}")
    print()

    # Bayeux frames are arrays of messages; index one example per channel per
    # direction, since that pairing is what a client has to reproduce.
    examples: OrderedDict[tuple, dict] = OrderedDict()
    channel_counts: Counter = Counter()

    for f in frames:
        parsed = f.get("parsed")
        if parsed is None:
            continue
        sent = "Sent" in f.get("method", "")
        msgs = parsed if isinstance(parsed, list) else [parsed]
        for m in msgs:
            if not isinstance(m, dict):
                continue
            ch = m.get("channel", "(no channel)")
            key = (ch, sent)
            channel_counts[key] += 1
            if key not in examples:
                examples[key] = m

    print("=" * 70)
    print("CHANNELS")
    print("=" * 70)
    for (ch, sent), n in sorted(channel_counts.items(), key=lambda kv: -kv[1]):
        print(f"  {'-->' if sent else '<--'} {ch:<46} {n}")
    print()

    print("=" * 70)
    print("ONE EXAMPLE PER CHANNEL / DIRECTION")
    print("=" * 70)
    for (ch, sent), m in examples.items():
        arrow = "CLIENT -> SERVER" if sent else "SERVER -> CLIENT"
        print(f"\n--- {arrow}   channel: {ch} ---")
        print(json.dumps(scrub(m), indent=2, ensure_ascii=False)[:2400])

    # The two unknowns that block a subscriber implementation.
    print("\n" + "=" * 70)
    print("BLOCKERS")
    print("=" * 70)
    subs = sorted(
        {
            m.get("subscription")
            for f in frames
            if isinstance(f.get("parsed"), list)
            for m in f["parsed"]
            if isinstance(m, dict) and m.get("subscription")
        }
    )
    print("\nsubscription channels seen:")
    for s in subs:
        print(f"  {s}")
    if not subs:
        print("  (none)")

    has_dm = any(s and not s.startswith("/group/") for s in subs)
    print(f"\nDM channel naming resolved: {'YES' if has_dm else 'NO -- only /group/ seen'}")

    inbound = [
        m
        for f in frames
        if not f.get("parsed") is None and "Received" in f.get("method", "")
        for m in (f["parsed"] if isinstance(f["parsed"], list) else [f["parsed"]])
        if isinstance(m, dict) and m.get("data")
    ]
    print(f"inbound frames carrying `data`: {len(inbound)}")
    if inbound:
        print("\nfirst inbound payload with data:")
        print(json.dumps(scrub(inbound[0]), indent=2, ensure_ascii=False)[:2000])
    else:
        print("  NONE -- no message actually arrived while recording.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
