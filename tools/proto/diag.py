# -*- coding: utf-8 -*-
"""passToken 换取诊断：只打印 status/字段名/掩码值，不泄露凭据。"""
import json

import requests

import mijia_proto as mp

cache = json.load(open("pass_token.json", encoding="utf-8"))
print("cache keys:", list(cache.keys()))
pt = cache.get("passToken") or ""
print("passToken len:", len(pt or ""))
uid = cache.get("userId")
print("userId:", uid)

sess = requests.Session()
sess.headers["User-Agent"] = mp.UA
sess.cookies.set("passToken", pt, domain=".xiaomi.com", path="/")
if uid:
    sess.cookies.set("userId", str(uid), domain=".xiaomi.com", path="/")

r = sess.get("https://account.xiaomi.com/pass/serviceLogin",
             params={"sid": "xiaomiio", "_json": "true"}, timeout=15)
print("status:", r.status_code)
print("final url:", r.url)
print("jar after get:", {k: (v[:6] + "...") for k, v in sess.cookies.items()})
text = mp._strip_jsonp(r.text)
print("body len:", len(text))
print("body[:400]:", repr(text[:400]))
try:
    data = json.loads(text)
    print("json keys:", list(data.keys()))
    masked = {k: (str(v)[:8] + "...") if k in mp._SENSITIVE and v else v
              for k, v in data.items()}
    print("masked:", masked)
except ValueError as e:
    print("not json:", e)
