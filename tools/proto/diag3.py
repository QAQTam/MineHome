# -*- coding: utf-8 -*-
"""diag3: 探查 serviceLogin(passToken) 的 nonce 类型 + 两种 clientSign 编码的 sts 行为。"""
import base64
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
print("code:", step.get("code"))
print("nonce repr:", repr(step.get("nonce")), "type:", type(step.get("nonce")).__name__)
print("ssecurity repr[:16]:", repr(str(step.get("ssecurity"))[:16]))
print("userId:", step.get("userId"), "type:", type(step.get("userId")).__name__)

for label, cs in (
    ("nonce数字", json.dumps({"nonce": step["nonce"], "ssecurity": step["ssecurity"]},
                             separators=(",", ":"))),
    ("nonce字符串", json.dumps({"nonce": str(step["nonce"]), "ssecurity": step["ssecurity"]},
                               separators=(",", ":"))),
):
    sess2 = requests.Session()
    sess2.headers["User-Agent"] = mp.UA
    for k, v in sess.cookies.items():
        sess2.cookies.set(k, v, domain=".xiaomi.com", path="/")
    sign = base64.b64encode(cs.encode()).decode()
    url = step["location"] + "&clientSign=" + requests.utils.quote(sign)
    r2 = sess2.get(url, timeout=15, allow_redirects=False)
    sc = r2.headers.get("Set-Cookie")
    print("\n[%s] status=%s Set-Cookie=%s" % (label, r2.status_code,
                                              (sc[:150] + "...") if sc else None))
    print("    jar keys:", list(sess2.cookies.keys()))
