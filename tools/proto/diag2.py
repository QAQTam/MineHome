# -*- coding: utf-8 -*-
"""diag2: 追踪 serviceLogin(passToken) -> clientSign 全链路，找 serviceToken 下发点。"""
import json

import requests

import mijia_proto as mp

cache = json.load(open("pass_token.json", encoding="utf-8"))
pt, uid = cache["passToken"], cache.get("userId")

sess = requests.Session()
sess.headers["User-Agent"] = mp.UA
sess.cookies.set("passToken", pt, domain=".xiaomi.com", path="/")
sess.cookies.set("userId", str(uid), domain=".xiaomi.com", path="/")

r = sess.get("https://account.xiaomi.com/pass/serviceLogin",
             params={"sid": "xiaomiio", "_json": "true"}, timeout=15)
step = json.loads(mp._strip_jsonp(r.text))
print("serviceLogin keys:", sorted(step.keys()))
print("  code:", step.get("code"), "| ssecurity:", bool(step.get("ssecurity")),
      "| serviceToken 字段:", bool(step.get("serviceToken")))
print("  location host:", step.get("location", "")[:80])

# 重定向链路逐跳看 Set-Cookie
url = step["location"] + "&clientSign=" + requests.utils.quote(
    __import__("base64").b64encode(json.dumps(
        {"nonce": step["nonce"], "ssecurity": step["ssecurity"]},
        separators=(",", ":")).encode()).decode())

r2 = sess.get(url, timeout=20, allow_redirects=False)
print("\n== 第一跳: status=%s" % r2.status_code)
print("   Location:", r2.headers.get("Location", "")[:100])
sc = r2.headers.get("Set-Cookie")
print("   Set-Cookie:", (sc[:120] + "...") if sc else None)
print("   body[:60]:", repr(r2.text[:60]))

hop, n = r2, 0
while n < 6 and 300 <= hop.status_code < 400:
    n += 1
    nxt = hop.headers["Location"]
    r3 = sess.get(nxt, timeout=20, allow_redirects=False)
    sc = r3.headers.get("Set-Cookie")
    print("== 跳%d: %s -> %s | Set-Cookie=%s | body=%r" % (
        n, hop.status_code, nxt[:60], (sc[:100] + "...") if sc else None, r3.text[:40]))
    hop = r3

print("\n最终 jar keys:", list(sess.cookies.keys()))
print("serviceToken in jar:", bool(sess.cookies.get("serviceToken")))
