#!/usr/bin/env python3
"""Audit Phase 2C/2D/2E/2F: concurrency, bridge security, pairing races, fuzzing.
Runs against a bridge started by the caller (see audit_concurrency_bridge.sh).
All test data in fresh /tmp dirs. Prints AUDIT-RESULT lines.
"""
import json, os, subprocess, sys, time, urllib.request, urllib.error, threading, socket

BASE = sys.argv[1]  # e.g. http://127.0.0.1:18832
TOKEN = sys.argv[2]
ROOT = sys.argv[3]

def req(method, path, body=None, headers=None, timeout=10):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method)
    if headers:
        for k, v in headers.items():
            r.add_header(k, v)
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            return resp.status, resp.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)

def auth():
    return {"X-FetchForge-Token": TOKEN, "Content-Type": "application/json"}

def list_dl():
    s, b = req("GET", "/downloads", headers=auth())
    return json.loads(b) if s == 200 else []

print("=== C1 rapid adds in same second: ID collisions ===")
ids = []
for i in range(20):
    s, b = req("POST", "/downloads", body={"url": f"http://127.0.0.1:18831/slow.bin", "filename": f"rapid{i}.bin"}, headers=auth())
    try:
        ids.append(json.loads(b)["id"])
    except Exception:
        ids.append(f"ERR{s}:{b[:80]}")
print("AUDIT-RESULT rapid-adds ids_unique=", len(set(ids)), "of", len(ids), "ids=", ids[:5])
recs = list_dl()
print("AUDIT-RESULT rapid-adds records_in_list=", len(recs), "unique_record_ids=", len(set(r["id"] for r in recs)))

print("=== C2 pause/resume/cancel spam on one job ===")
# Use the first rapid job if any, else create one
if recs:
    jid = recs[0]["id"]
    for i in range(30):
        action = ["pause", "resume", "cancel"][i % 3]
        req("POST", f"/downloads/{jid}/{action}", headers=auth())
    time.sleep(2)
    s, b = req("GET", "/health")
    print("AUDIT-RESULT spam health=", s, "list_after_spam=", [(r["id"], r["status"]) for r in list_dl() if r["id"] == jid])

print("=== C3 bandwidth accuracy (100 kbps, 200 KiB file via slow server?) ===")
print("AUDIT-RESULT bandwidth SKIPPED_LIVE (see code review: per-segment limiter divides by 4 regardless of segment count; full live test in report notes)")

print("=== D1 huge body, malformed JSON, wrong content-type ===")
s, b = req("POST", "/downloads", body={"url": "x"}, headers={"X-FetchForge-Token": TOKEN, "Content-Type": "text/plain"})
print("AUDIT-RESULT wrong-content-type status=", s, "body=", b[:120])
# Raw huge body via socket
try:
    sock = socket.create_connection(("127.0.0.1", int(BASE.rsplit(":", 1)[1])), timeout=10)
    big = b'{"url": "' + b"A" * (5 * 1024 * 1024) + b'"}'
    sock.sendall(f"POST /downloads HTTP/1.1\r\nHost: 127.0.0.1\r\nX-FetchForge-Token: {TOKEN}\r\nContent-Length: {len(big)}\r\n\r\n".encode() + big)
    resp = sock.recv(4096).decode(errors="replace")
    print("AUDIT-RESULT huge-body-5MB response_start=", resp[:200].replace("\r", " ").replace("\n", " "))
    sock.close()
except Exception as e:
    print("AUDIT-RESULT huge-body-5MB error=", e)
s, b = req("GET", "/health")
print("AUDIT-RESULT bridge-alive-after-huge-body health=", s)

print("=== D2 request flooding: 200 rapid /downloads GETs ===")
ok = 0
for _ in range(200):
    s, _ = req("GET", "/downloads", headers=auth(), timeout=5)
    if s == 200:
        ok += 1
print("AUDIT-RESULT flood 200 requests ok=", ok, " (no rate limit expected)")

print("=== D3 CRLF / control chars in filename via API ===")
for fn in ["a\r\nb.txt", "a\nb.txt", "x\x00y.txt", "a:b.txt"]:
    s, b = req("POST", "/downloads", body={"url": "http://127.0.0.1:18831/slow.bin", "filename": fn}, headers=auth())
    print("AUDIT-RESULT crlf-filename input=", repr(fn), "status=", s, "body=", b[:200])

print("=== D4 CORS preflight ===")
s, b = req("OPTIONS", "/downloads", headers={"Origin": "https://evil.example", "Access-Control-Request-Method": "POST", "Access-Control-Request-Headers": "X-FetchForge-Token"})
print("AUDIT-RESULT cors-preflight status=", s, "body=", b[:120])
# Raw OPTIONS to see headers
try:
    sock = socket.create_connection(("127.0.0.1", int(BASE.rsplit(":", 1)[1])), timeout=5)
    sock.sendall(b"OPTIONS /downloads HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: X-FetchForge-Token\r\n\r\n")
    resp = sock.recv(4096).decode(errors="replace")
    print("AUDIT-RESULT cors-raw-headers=", resp.replace("\r", " ").replace("\n", " ")[:500])
    sock.close()
except Exception as e:
    print("AUDIT-RESULT cors-raw error=", e)

print("=== E1 pairing race: two concurrent verifies with the same correct code ===")
s, b = req("POST", "/pair/generate", headers=auth())
code = json.loads(b).get("code", "") if s == 200 else ""
print("AUDIT-RESULT pair-generate status=", s, "code_present=", bool(code))
results = []
def verify():
    ss, bb = req("POST", "/pair/verify", body={"code": code})
    results.append((ss, bb[:120]))
threads = [threading.Thread(target=verify) for _ in range(2)]
for t in threads: t.start()
for t in threads: t.join()
print("AUDIT-RESULT pair-race results=", results, " (exactly one should succeed)")

print("=== F1 fuzz URLs/filenames/settings via API (200 random-ish inputs) ===")
import random, string
random.seed(42)
crashes = 0
for i in range(200):
    choice = random.choice(["url", "filename", "both"])
    url = "http://127.0.0.1:18831/slow.bin"
    fn = f"fuzz{i}.bin"
    if choice in ("url", "both"):
        url = random.choice([
            "http://127.0.0.1:18831/" + "".join(random.choices(string.printable[:90], k=random.randint(0, 50))),
            "https://example.com/" + "é🎉تقرير" * random.randint(1, 5) + ".pdf",
            "http://" + "a" * random.randint(1, 200) + ".com/x",
            "http://127.0.0.1:18831/slow.bin?" + "x" * 5000,
        ])
    if choice in ("filename", "both"):
        fn = "".join(random.choices("abcXYZ019 _-<>:/\\|?*🎉تقرير\n\r", k=random.randint(0, 30))) or "fuzz.bin"
    s, b = req("POST", "/downloads", body={"url": url, "filename": fn}, headers=auth(), timeout=5)
    if s == 0:
        crashes += 1
s, _ = req("GET", "/health")
print("AUDIT-RESULT fuzz 200 inputs bridge_health_after=", s, "connection_errors=", crashes)
print("AUDIT-CONCURRENCY-BRIDGE DONE")
