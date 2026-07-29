"""Capture live GroupMe traffic through selenium-wire.

Launches Chrome behind selenium-wire's MITM proxy, waits for you to log in at
web.groupme.com, and records EVERYTHING for every *.groupme.com request:
request headers, cookies, request payloads, response headers, response bodies.

    !!  THE OUTPUT CONTAINS LIVE CREDENTIALS  !!

    capture-out/ holds your x-access-token, session cookies, and the full
    plaintext of every message fetched while recording. It is gitignored.
    Do not commit it, paste it, or attach it to an issue. Revoke the token at
    https://dev.groupme.com/applications when you are finished.

Usage:
    py tools/capture_api.py              # runs until you close Chrome / Ctrl-C
    py tools/capture_api.py --timeout 0  # explicit: no time limit

Output (tools/capture-out/):
    traffic.jsonl   one JSON object per request, written as it happens
    endpoints.json  deduplicated endpoint catalogue
    endpoints.md    human-readable summary
    cookies.json    browser cookie jar at exit
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from collections import defaultdict
from pathlib import Path

OUT_DIR = Path(__file__).parent / "capture-out"
PROFILE_DIR = Path(__file__).parent / ".chrome-profile"

TARGET_HOSTS = (
    "groupme.com",
    "gm.groupme.com",
)

# Bodies above this are truncated so one video upload cannot produce a 400 MB
# log. Plenty for any JSON payload.
MAX_BODY = 2_000_000


def patch_selenium_wire() -> None:
    """Make selenium-wire's vendored mitmproxy work with modern pyOpenSSL.

    selenium-wire 5.1.0 has been unmaintained since 2023 and ships a mitmproxy
    fork built against pyOpenSSL's legacy X509 API. pyOpenSSL 23.3 deleted that
    API wholesale, so on a current install every single TLS interception dies
    and the browser loads nothing at all:

        create_ca()   -> X509.add_extensions()      (removed)
                         X509Extension              (removed)
        dummy_cert()  -> X509.add_extensions()      (removed)
        Cert.altnames -> X509.get_extension_count() (removed)

    All three are reimplemented below on ``cryptography``, which is what
    pyOpenSSL wraps anyway, then converted back to the pyOpenSSL objects the
    rest of mitmproxy expects.

    Patched at runtime rather than by pinning pyOpenSSL back below 23.3: this
    process is the only thing that wants the old behaviour, and downgrading a
    system-wide crypto library to satisfy one dev tool is the wrong trade.
    """
    import datetime as _dt
    import ipaddress as _ip
    import os
    import time as _time

    import OpenSSL.crypto as _ossl
    from cryptography import x509 as cx509
    from cryptography.hazmat.primitives import hashes as _hashes
    from cryptography.hazmat.primitives import serialization as _serialization
    from cryptography.hazmat.primitives.asymmetric import rsa as _rsa
    from cryptography.hazmat.primitives.serialization import pkcs12 as _pkcs12
    from cryptography.x509.oid import ExtendedKeyUsageOID, ExtensionOID, NameOID
    from seleniumwire.thirdparty.mitmproxy import certs as sw_certs

    def _s(v):
        return v.decode("utf-8") if isinstance(v, (bytes, bytearray)) else v

    def altnames(self):
        names: list[bytes] = []
        try:
            ext = self.x509.to_cryptography().extensions.get_extension_for_oid(
                ExtensionOID.SUBJECT_ALTERNATIVE_NAME
            )
            for entry in ext.value:
                if isinstance(entry, cx509.DNSName):
                    names.append(entry.value.encode())
                elif isinstance(entry, cx509.IPAddress):
                    names.append(str(entry.value).encode())
        except Exception:  # noqa: BLE001 - a cert with no SAN is normal
            pass
        return names

    def create_ca(organization, cn, exp, key_size):
        key = _rsa.generate_private_key(public_exponent=65537, key_size=key_size)
        name = cx509.Name(
            [
                cx509.NameAttribute(NameOID.COMMON_NAME, _s(cn)),
                cx509.NameAttribute(NameOID.ORGANIZATION_NAME, _s(organization)),
            ]
        )
        now = _dt.datetime.now(_dt.timezone.utc)
        cert = (
            cx509.CertificateBuilder()
            .subject_name(name)
            .issuer_name(name)
            .public_key(key.public_key())
            .serial_number(int(_time.time() * 10000))
            .not_valid_before(now - _dt.timedelta(hours=48))
            .not_valid_after(now + _dt.timedelta(seconds=exp))
            .add_extension(cx509.BasicConstraints(ca=True, path_length=None), True)
            .add_extension(
                cx509.KeyUsage(
                    digital_signature=False,
                    content_commitment=False,
                    key_encipherment=False,
                    data_encipherment=False,
                    key_agreement=False,
                    key_cert_sign=True,
                    crl_sign=True,
                    encipher_only=False,
                    decipher_only=False,
                ),
                True,
            )
            .add_extension(
                cx509.ExtendedKeyUsage(
                    [
                        ExtendedKeyUsageOID.SERVER_AUTH,
                        ExtendedKeyUsageOID.CLIENT_AUTH,
                        ExtendedKeyUsageOID.EMAIL_PROTECTION,
                        ExtendedKeyUsageOID.TIME_STAMPING,
                        ExtendedKeyUsageOID.CODE_SIGNING,
                    ]
                ),
                False,
            )
            # nsCertType is dropped: it is a dead Netscape extension with no
            # cryptography equivalent, and every current browser ignores it.
            .add_extension(
                cx509.SubjectKeyIdentifier.from_public_key(key.public_key()), False
            )
            .sign(key, _hashes.SHA256())
        )
        return (
            _ossl.PKey.from_cryptography_key(key),
            _ossl.X509.from_cryptography(cert),
        )

    def dummy_cert(privkey, cacert, commonname, sans, organization):
        ca = cacert.to_cryptography()
        ca_key = privkey.to_cryptography_key()

        valid_cn = commonname is not None and len(commonname) < 64
        attrs = []
        if valid_cn:
            attrs.append(cx509.NameAttribute(NameOID.COMMON_NAME, _s(commonname)))
        if organization is not None:
            attrs.append(
                cx509.NameAttribute(NameOID.ORGANIZATION_NAME, _s(organization))
            )

        alt = []
        for entry in sans or []:
            text = _s(entry)
            try:
                alt.append(cx509.IPAddress(_ip.ip_address(text)))
            except ValueError:
                alt.append(cx509.DNSName(text))

        now = _dt.datetime.now(_dt.timezone.utc)
        builder = (
            cx509.CertificateBuilder()
            .subject_name(cx509.Name(attrs))
            .issuer_name(ca.subject)
            # Leaf shares the CA keypair, matching upstream mitmproxy.
            .public_key(ca.public_key())
            .serial_number(int(_time.time() * 10000))
            .not_valid_before(now - _dt.timedelta(hours=48))
            .not_valid_after(
                now + _dt.timedelta(seconds=sw_certs.DEFAULT_EXP_DUMMY_CERT)
            )
            .add_extension(
                cx509.ExtendedKeyUsage(
                    [ExtendedKeyUsageOID.SERVER_AUTH, ExtendedKeyUsageOID.CLIENT_AUTH]
                ),
                False,
            )
        )
        if alt:
            # RFC 5280 4.2.1.6: SAN is critical when the subject is empty.
            builder = builder.add_extension(
                cx509.SubjectAlternativeName(alt), not valid_cn
            )
        return sw_certs.Cert(
            _ossl.X509.from_cryptography(builder.sign(ca_key, _hashes.SHA256()))
        )

    def create_store(
        path,
        basename,
        key_size,
        organization=None,
        cn=None,
        expiry=sw_certs.DEFAULT_EXP,
    ):
        """Same layout as upstream, minus the removed ``OpenSSL.crypto.PKCS12``.

        Only ``-ca.pem`` and ``-dhparam.pem`` are read back by the proxy; the
        ``.p12`` files exist so a user can install the CA into the Windows or
        Android trust store. Regenerated through ``cryptography``'s pkcs12
        serializer instead of being dropped, so that stays possible.
        """
        os.makedirs(path, exist_ok=True)
        organization = organization or basename
        cn = cn or basename

        key, ca = create_ca(
            organization=organization, cn=cn, exp=expiry, key_size=key_size
        )
        key_pem = _ossl.dump_privatekey(_ossl.FILETYPE_PEM, key)
        ca_pem = _ossl.dump_certificate(_ossl.FILETYPE_PEM, ca)

        with sw_certs.CertStore.umask_secret(), open(
            os.path.join(path, basename + "-ca.pem"), "wb"
        ) as f:
            f.write(key_pem)
            f.write(ca_pem)

        for suffix in ("-ca-cert.pem", "-ca-cert.cer"):
            with open(os.path.join(path, basename + suffix), "wb") as f:
                f.write(ca_pem)

        ca_c = ca.to_cryptography()
        key_c = key.to_cryptography_key()
        with open(os.path.join(path, basename + "-ca-cert.p12"), "wb") as f:
            f.write(
                _pkcs12.serialize_key_and_certificates(
                    name=None,
                    key=None,
                    cert=ca_c,
                    cas=None,
                    encryption_algorithm=_serialization.NoEncryption(),
                )
            )
        with sw_certs.CertStore.umask_secret(), open(
            os.path.join(path, basename + "-ca.p12"), "wb"
        ) as f:
            f.write(
                _pkcs12.serialize_key_and_certificates(
                    name=None,
                    key=key_c,
                    cert=ca_c,
                    cas=None,
                    encryption_algorithm=_serialization.NoEncryption(),
                )
            )

        with open(os.path.join(path, basename + "-dhparam.pem"), "wb") as f:
            f.write(sw_certs.DEFAULT_DHPARAM)

        return key, ca

    sw_certs.Cert.altnames = property(altnames)
    sw_certs.create_ca = create_ca
    sw_certs.dummy_cert = dummy_cert
    sw_certs.CertStore.create_store = staticmethod(create_store)

    print("[*] Patched mitmproxy certs for pyOpenSSL >= 23.3", flush=True)


def path_template(path: str) -> str:
    """/v3/groups/12345678/messages -> /v3/groups/{id}/messages"""
    out = []
    for seg in (path or "/").split("?")[0].split("/"):
        if seg.isdigit() and len(seg) >= 4:
            out.append("{id}")
        elif re.fullmatch(r"[0-9a-f]{16,}", seg or "", re.I):
            out.append("{hash}")
        else:
            out.append(seg)
    return "/".join(out)


def body_text(raw: bytes | None, headers) -> str | None:
    if not raw:
        return None
    try:
        from seleniumwire.utils import decode

        enc = headers.get("Content-Encoding", "identity") if headers else "identity"
        raw = decode(raw, enc)
    except Exception:  # noqa: BLE001
        pass
    try:
        text = raw.decode("utf-8", errors="replace")
    except Exception:  # noqa: BLE001
        return f"<{len(raw)} bytes binary>"
    if len(text) > MAX_BODY:
        return text[:MAX_BODY] + f"\n<truncated, {len(text)} chars total>"
    return text


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--timeout",
        type=int,
        default=0,
        help="max seconds; 0 = run until the browser closes",
    )
    ap.add_argument("--url", default="https://web.groupme.com")
    ap.add_argument(
        "--force-longpoll",
        action="store_true",
        help="disable WebSocket so Faye realtime falls back to visible HTTP polling",
    )
    args = ap.parse_args()

    try:
        patch_selenium_wire()
        from seleniumwire import webdriver
    except Exception as e:  # noqa: BLE001
        print(f"[!] selenium-wire setup failed: {e}", flush=True)
        return 2

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    PROFILE_DIR.mkdir(parents=True, exist_ok=True)
    traffic_path = OUT_DIR / "traffic.jsonl"
    traffic_fh = traffic_path.open("a", encoding="utf-8")

    opts = webdriver.ChromeOptions()
    # Dedicated profile: the login persists between runs and the user's real
    # Chrome profile is never touched.
    opts.add_argument(f"--user-data-dir={PROFILE_DIR}")
    opts.add_argument("--no-first-run")
    opts.add_argument("--no-default-browser-check")
    opts.add_argument("--start-maximized")
    opts.add_argument("--ignore-certificate-errors")

    # CDP performance logging, for websocket frames.
    #
    # GroupMe's realtime layer is Faye over a websocket. selenium-wire proxies
    # HTTP and cannot see inside an upgraded connection, and forcing the
    # long-polling fallback does not work either: GroupMe's own edge returns
    # nginx 504s on the held request, because their client always upgrades and
    # the long-poll path is not really supported.
    #
    # CDP sees the frames directly -- Network.webSocketFrameSent /
    # webSocketFrameReceived carry payloadData with no interception at all.
    opts.set_capability("goog:loggingPrefs", {"performance": "ALL", "browser": "ALL"})

    sw_opts = {
        "suppress_connection_errors": True,
        "verify_ssl": False,
        "disable_encoding": True,  # ask servers for plaintext; simpler bodies
    }

    print("[*] Launching Chrome behind the selenium-wire proxy...", flush=True)
    driver = webdriver.Chrome(options=opts, seleniumwire_options=sw_opts)

    if args.force_longpoll:
        # GroupMe's realtime layer is Faye (Bayeux). Its handshake happens over
        # JSONP, but the client lists "websocket" first in
        # supportedConnectionTypes and upgrades immediately -- after which every
        # subscribe and every inbound message is a websocket frame, invisible to
        # an HTTP-level proxy.
        #
        # Faye negotiates transports by feature detection, so removing
        # WebSocket and EventSource before any page script runs makes it fall
        # back to cross-origin-long-polling: ordinary HTTP requests that this
        # capture records in full.
        #
        # Cost: realtime gets slower and chattier for this session. That is
        # fine -- the point is to read the protocol, not to use the app.
        driver.execute_cdp_cmd(
            "Page.addScriptToEvaluateOnNewDocument",
            {
                "source": """
                (function () {
                  try {
                    delete window.WebSocket;
                    Object.defineProperty(window, 'WebSocket',
                      { get: function () { return undefined; }, configurable: true });
                  } catch (e) {}
                  try {
                    delete window.EventSource;
                    Object.defineProperty(window, 'EventSource',
                      { get: function () { return undefined; }, configurable: true });
                  } catch (e) {}
                  console.log('[capture] WebSocket disabled; Faye will long-poll');
                })();
                """
            },
        )
        print(
            "[*] WebSocket disabled -- Faye will fall back to long-polling,\n"
            "    so /meta/subscribe and inbound message frames become visible.",
            flush=True,
        )

    driver.get(args.url)

    print("=" * 70, flush=True)
    print("  LOG IN to GroupMe in the Chrome window that just opened.", flush=True)
    print("", flush=True)
    print("  REALTIME (the blocking one -- websocket frames):", flush=True)
    print("    - leave a group open and HAVE SOMEONE SEND YOU A MESSAGE", flush=True)
    print("      (or send yourself one from your phone)", flush=True)
    print("    - do the same in a DM -- the DM channel name is still unknown", flush=True)
    print("    - have someone react to a message while you watch", flush=True)
    print("", flush=True)
    print("  WRITES not yet captured (needed for a full client):", flush=True)
    print("    - upload an image, and a file if you can", flush=True)
    print("    - create a poll, then vote in it", flush=True)
    print("    - create an event, then RSVP", flush=True)
    print("    - start typing and pause (typing indicator)", flush=True)
    print("    - change a group's name/avatar; add or remove a member", flush=True)
    print("", flush=True)
    print("  ALREADY COVERED -- no need to repeat:", flush=True)
    print("    history paging, send, reply, react, delete, edit, pin/unpin", flush=True)
    print("", flush=True)
    print("  The browser stays open. Close it when done, or press Ctrl-C.", flush=True)
    print("  WARNING: capture-out/ will contain your live token + messages.", flush=True)
    print("=" * 70, flush=True)

    endpoints: dict[tuple, dict] = {}
    seen: set[str] = set()
    started = time.time()
    last_report = 0.0

    ws_path = OUT_DIR / "websocket.jsonl"
    ws_fh = ws_path.open("a", encoding="utf-8")
    ws_count = [0]

    def harvest_websocket():
        """Drain CDP performance logs for websocket frames.

        Faye multiplexes handshake, subscribe, connect and every inbound message
        over one socket, so these frames are the entire realtime protocol. The
        log is consumed destructively by `get_log`, so this must keep up.
        """
        try:
            entries = driver.get_log("performance")
        except Exception:  # noqa: BLE001 - logging unavailable is not fatal
            return
        for entry in entries:
            try:
                msg = json.loads(entry["message"])["message"]
            except Exception:  # noqa: BLE001
                continue
            method = msg.get("method", "")
            if not method.startswith("Network.webSocket"):
                continue
            params = msg.get("params", {})
            payload = (params.get("response") or {}).get("payloadData")
            record = {
                "ts": entry.get("timestamp"),
                "method": method,
                "url": params.get("url"),
                "requestId": params.get("requestId"),
                "payload": payload,
            }
            # Frames arrive as a JSON string; parse so the shape is readable
            # rather than an escaped blob.
            if payload:
                try:
                    record["parsed"] = json.loads(payload)
                except Exception:  # noqa: BLE001
                    pass
            ws_fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            ws_fh.flush()
            ws_count[0] += 1

    def harvest():
        for req in driver.requests:
            if req.id in seen:
                continue
            host = (req.host or "").lower()
            if not any(host == h or host.endswith("." + h) for h in TARGET_HOSTS):
                continue
            seen.add(req.id)

            resp = req.response
            record = {
                "ts": getattr(req, "date", None).isoformat()
                if getattr(req, "date", None)
                else None,
                "method": req.method,
                "url": req.url,
                "host": host,
                "path": req.path,
                "querystring": req.querystring,
                "request_headers": dict(req.headers),
                "request_body": body_text(req.body, req.headers),
                "status": resp.status_code if resp else None,
                "response_headers": dict(resp.headers) if resp else None,
                "response_body": body_text(resp.body, resp.headers) if resp else None,
            }
            traffic_fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            traffic_fh.flush()

            key = (req.method, host, path_template(req.path))
            entry = endpoints.setdefault(
                key,
                {
                    "method": req.method,
                    "host": host,
                    "path": path_template(req.path),
                    "count": 0,
                    "query_keys": set(),
                    "statuses": set(),
                    "request_header_names": set(),
                    "content_types": set(),
                },
            )
            entry["count"] += 1
            entry["request_header_names"].update(req.headers.keys())
            for pair in (req.querystring or "").split("&"):
                if pair:
                    entry["query_keys"].add(pair.split("=", 1)[0])
            if resp:
                entry["statuses"].add(resp.status_code)
                ct = resp.headers.get("Content-Type")
                if ct:
                    entry["content_types"].add(ct.split(";")[0].strip())

        if len(driver.requests) > 2500:
            del driver.requests

    def write_summary():
        rows = []
        for e in endpoints.values():
            rows.append(
                {
                    "method": e["method"],
                    "host": e["host"],
                    "path": e["path"],
                    "count": e["count"],
                    "query_keys": sorted(e["query_keys"]),
                    "statuses": sorted(e["statuses"]),
                    "request_header_names": sorted(e["request_header_names"]),
                    "content_types": sorted(e["content_types"]),
                }
            )
        rows.sort(key=lambda r: (r["host"], r["path"], r["method"]))
        (OUT_DIR / "endpoints.json").write_text(
            json.dumps(rows, indent=2), encoding="utf-8"
        )

        lines = [
            "# Captured GroupMe endpoints",
            "",
            f"{len(rows)} distinct endpoints across {len(seen)} requests.",
            "",
            "Full request/response detail is in `traffic.jsonl`.",
            "",
        ]
        by_host = defaultdict(list)
        for r in rows:
            by_host[r["host"]].append(r)
        for host in sorted(by_host):
            lines += [f"## {host}", "", "| Method | Path | Hits | Status | Query params |", "|---|---|---|---|---|"]
            for r in by_host[host]:
                q = ", ".join(f"`{k}`" for k in r["query_keys"]) or "—"
                st = ", ".join(str(s) for s in r["statuses"]) or "—"
                lines.append(f"| `{r['method']}` | `{r['path']}` | {r['count']} | {st} | {q} |")
            lines.append("")
        (OUT_DIR / "endpoints.md").write_text("\n".join(lines), encoding="utf-8")
        return rows

    try:
        while True:
            if args.timeout and (time.time() - started) > args.timeout:
                print("[*] Timeout reached.", flush=True)
                break
            try:
                _ = driver.current_url  # raises once the window is gone
            except Exception:  # noqa: BLE001
                print("[*] Browser closed.", flush=True)
                break
            harvest()
            harvest_websocket()
            now = time.time()
            if now - last_report > 15:
                last_report = now
                write_summary()
                print(
                    f"[*] {len(endpoints)} endpoints | {len(seen)} requests | "
                    f"{ws_count[0]} ws frames | {int(now - started)}s",
                    flush=True,
                )
            time.sleep(2)
    except KeyboardInterrupt:
        print("[*] Interrupted.", flush=True)
    finally:
        try:
            harvest()
            harvest_websocket()
            ws_fh.close()
            print(f"[+] {ws_count[0]} websocket frames -> {ws_path}", flush=True)
        except Exception:  # noqa: BLE001
            pass
        try:
            cookies = driver.get_cookies()
            (OUT_DIR / "cookies.json").write_text(
                json.dumps(cookies, indent=2), encoding="utf-8"
            )
            print(f"[+] {len(cookies)} cookies -> cookies.json", flush=True)
        except Exception as e:  # noqa: BLE001
            print(f"[!] cookie dump skipped: {e}", flush=True)
        rows = write_summary()
        traffic_fh.close()
        try:
            driver.quit()
        except Exception:  # noqa: BLE001
            pass
        print(f"[+] {len(rows)} endpoints, {len(seen)} requests", flush=True)
        print(f"[+] {traffic_path}", flush=True)
        print(f"[+] {OUT_DIR / 'endpoints.md'}", flush=True)

    return 0


if __name__ == "__main__":
    sys.exit(main())
