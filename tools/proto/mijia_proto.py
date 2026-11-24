# -*- coding: utf-8 -*-
"""
米家云 API 最小验证原型（Python）
================================

算法来源：本仓库对 com.xiaomi.smarthome v11.9.705 的静态还原
  - 登录时序   : com/xiaomi/accountsdk/account/URLs.java + XMPassport.java
  - RC4 签名   : CloudVideoNetUtils.paramEncrypt() + _m_j/ys2.java + _m_j/g7c.java
                 + _m_j/ew0.java + _m_j/jm0.java（详见 协议面分析报告.md §3.1）
  - 控制端点   : _m_j/fgb.java (/home/rpc/<did>, /home/rpcv2/<did>)

签名协议（Crypto.RC4 模式，与 app 逐行一致）：
  sessionSecurity = base64( SHA256( b64decode(ssecurity) + b64decode(nonce) ) )   # 32B
  nonce  = base64( int64_be(random) + int32_be((now_ms + offset)//60000) )        # 12B -> 16char
  sign(method, path, params, sessionSecurity):
      "&".join([ METHOD, path, *sorted("k=v"), sessionSecurity ]) -> base64( SHA1( utf8 ) )
  RC4: key=b64decode(sessionSecurity)，标准 KSA/PRGA，先 drop 1024 字节，
       同一 keystream 按 TreeMap 字典序连续 crypt 各字段（data -> rc4_hash__）
  form: data=<rc4(json)>, rc4_hash__=<rc4(b64(sha1(明文))), signature=<明文签名(密文)>,
        _nonce=<16char>, ssecurity=<明文>
  响应为明文 JSON（RC4 仅作用于请求体）。

用法：
  python mijia_proto.py login <user> <password>      # 登录，保存 session.json
  python mijia_proto.py devices                       # 设备列表（含 model/spec 简表）
  python mijia_proto.py switch <did> on|off           # spec 开关控制 (siid=2,piid=1)
  python mijia_proto.py rpcv2 <did> <method-json>     # 原始 /home/rpcv2/<did>
  python mijia_proto.py rpc <did> <json>              # 老 miio /home/rpc/<did>
  python mijia_proto.py raw <path> <json>             # 任意已签名 POST
依赖：pip install requests
"""
import base64
import hashlib
import json
import os
import random
import struct
import sys
import time

import requests

UA = ("Android-11.9.705-110917051" if False else
      "Android/11.9.705 (xiaomi.io; Android 15; 110917051) rf/passtoken/1.0.1")
SESSION_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "session.json")


# ---------------------------------------------------------------- crypto ----
def rc4(key: bytes, data: bytes) -> bytes:
    """标准 RC4（对应 _m_j/ew0，mode=3：标准 KSA + PRGA）。"""
    S = list(range(256))
    j = 0
    for i in range(256):
        j = (j + S[i] + key[i % len(key)]) & 255
        S[i], S[j] = S[j], S[i]
    i = j = 0
    out = bytearray(len(data))
    for n, b in enumerate(data):
        i = (i + 1) & 255
        j = (j + S[i]) & 255
        S[i], S[j] = S[j], S[i]
        out[n] = b ^ S[(S[i] + S[j]) & 255]
    return bytes(out)


class RC4Stream:
    """带状态的 RC4（同一条 keystream 连续 crypt，对应 _m_j/ew0）。"""

    def __init__(self, key: bytes):
        S = list(range(256))
        j = 0
        for i in range(256):
            j = (j + S[i] + key[i % len(key)]) & 255
            S[i], S[j] = S[j], S[i]
        self.S, self.i, self.j = S, 0, 0

    def crypt(self, data: bytes) -> bytes:
        S, out = self.S, bytearray()
        for b in data:
            self.i = (self.i + 1) & 255
            self.j = (self.j + S[self.i]) & 255
            S[self.i], S[self.j] = S[self.j], S[self.i]
            out.append(b ^ S[(S[self.i] + S[self.j]) & 255])
        return bytes(out)


def new_rc4_cipher(session_security_b64: str) -> RC4Stream:
    """g7c 复刻：key=b64decode(sessionSecurity)，构造后 drop 1024 字节。"""
    key = base64.b64decode(session_security_b64)
    assert len(key) == 32, "sessionSecurity must decode to 32 bytes"
    c = RC4Stream(key)
    c.crypt(bytes(1024))
    return c


def decrypt_response(session_security_b64: str, payload_b64: str) -> bytes:
    """micloud.decrypt_rc4：全新 RC4(b64d(sessionSecurity))，drop 1024，b64decode 后解密。"""
    c = RC4Stream(base64.b64decode(session_security_b64))
    c.crypt(bytes(1024))
    return c.crypt(base64.b64decode(payload_b64))


def gen_nonce(server_time_offset_ms: int = 0) -> str:
    """ys2.OooO00o：SecureRandom 8B + int32((now+offset)/60000)，大端，base64。"""
    rnd = struct.pack(">q", random.getrandbits(63) - (1 << 62))
    minute = int((int(time.time() * 1000) + server_time_offset_ms) / 60000)
    return base64.b64encode(rnd + struct.pack(">i", minute)).decode()


def session_security(ssecurity_b64: str, nonce_b64: str) -> str:
    """SHA256( b64decode(ssecurity) + b64decode(nonce) ) -> base64。"""
    digest = hashlib.sha256(
        base64.b64decode(ssecurity_b64) + base64.b64decode(nonce_b64)).digest()
    return base64.b64encode(digest).decode()


def sign(method: str, path: str, params: dict, session_sec: str) -> str:
    """ys2.OooO0O0：METHOD&path&k=v(排序)&sessionSecurity -> b64(SHA1)。"""
    parts = []
    if method:
        parts.append(method.upper())
    if path:
        parts.append(path)
    for k in sorted(params):
        parts.append("%s=%s" % (k, params[k]))
    parts.append(session_sec)
    raw = ("&".join(parts)).encode("utf-8")
    return base64.b64encode(hashlib.sha1(raw).digest()).decode()


# ---------------------------------------------------------------- session ---
def save_session(s: dict):
    with open(SESSION_FILE, "w", encoding="utf-8") as f:
        json.dump(s, f, indent=1)
    print("已保存", SESSION_FILE)


def load_session() -> dict:
    if not os.path.exists(SESSION_FILE):
        sys.exit("没有 session.json，请先: python mijia_proto.py login <user> <pass>")
    with open(SESSION_FILE, encoding="utf-8") as f:
        return json.load(f)


def _strip_jsonp(text: str) -> str:
    t = text.strip()
    if t.startswith("&&&START&&&"):
        t = t[len("&&&START&&&"):]
    return t


# ---------------------------------------------------------------- login -----
_SENSITIVE = {"ssecurity", "passToken", "nonce", "ticket", "serviceToken", "psecurity"}


def dump_masked(data: dict):
    """调试打印：敏感字段掩码，其余原样。"""
    for k, v in data.items():
        if k in _SENSITIVE and v:
            print("    %s = %s..." % (k, str(v)[:8]))
        else:
            print("    %s = %s" % (k, v))


def _finish_client_sign(sess, step: dict) -> dict:
    """直连/token 登录共用收尾：location + clientSign -> serviceToken。"""
    ssecurity = step["ssecurity"]
    # app 实现: XMPassport.getClientSign -> Coder.generateSignature(null,null,{"nonce":str},ssecurity)
    # = base64( SHA1( "nonce=<十进制>&" + ssecurity ) )
    raw = ("nonce=" + str(step["nonce"]) + "&" + ssecurity).encode("utf-8")
    client_sign = base64.b64encode(hashlib.sha1(raw).digest()).decode()
    url = (step["location"] + "&clientSign=" + requests.utils.quote(client_sign)
           + "&_userIdNeedEncrypt=true")
    r = sess.get(url, timeout=15)
    # serviceToken 在响应头 xiaomiio_serviceToken（sid 前缀），cookie jar 是兜底
    service_token = (r.headers.get("xiaomiio_serviceToken")
                     or r.headers.get("serviceToken"))
    try:
        final = json.loads(_strip_jsonp(r.text))
    except ValueError:
        final = {}
    if not service_token:
        service_token = (sess.cookies.get("xiaomiio_serviceToken")
                         or sess.cookies.get("serviceToken"))
    if not service_token:
        print("    [诊断] clientSign GET status=%s" % r.status_code)
        print("    [诊断] X-Xiaomi-Status-Code: %s" % r.headers.get("X-Xiaomi-Status-Code"))
        print("    [诊断] 响应头 keys: %s" % list(r.headers.keys()))
        print("    [诊断] cookie jar keys: %s" % list(sess.cookies.keys()))
    return {"userId": str(step.get("userId")),
            "ssecurity": ssecurity,
            "serviceToken": service_token,
            "cookies": dict(sess.cookies.items()),
            "final": final}


def _pass_token_login(sess, pass_token: str, user_id, server: str = "cn"):
    """B 路径：用 passToken 重放 serviceLogin，换取 ssecurity/nonce/location。"""
    sess.headers["User-Agent"] = UA
    # requests 手动 set cookie 必须带 domain，否则不会随请求发出
    sess.cookies.set("passToken", pass_token, domain=".xiaomi.com", path="/")
    if user_id:
        sess.cookies.set("userId", str(user_id), domain=".xiaomi.com", path="/")
    r = sess.get("https://account.xiaomi.com/pass/serviceLogin",
                 params={"sid": "xiaomiio", "_json": "true"}, timeout=15)
    text = _strip_jsonp(r.text)
    try:
        step = json.loads(text)
    except ValueError:
        print("    serviceLogin(passToken) 非JSON: status=%s" % r.status_code)
        print("    body[:300]: %r" % text[:300])
        raise
    if not step.get("ssecurity"):
        return None, step
    return _finish_client_sign(sess, step), None


def login(user: str, password: str, server: str = "cn"):
    """sid=xiaomiio 登录。
    A 直连: serviceLoginAuth2 直接给 ssecurity/nonce/location
    B 通知/新设备: auth2 只给 location/passToken -> 先 GET location 交换 cookie,
      再用 passToken 重放 serviceLogin 拿 ssecurity
    """
    sess = requests.Session()
    sess.headers["User-Agent"] = UA

    r1 = sess.get("https://account.xiaomi.com/pass/serviceLogin",
                  params={"sid": "xiaomiio", "_json": "true"}, timeout=15)
    step1 = json.loads(_strip_jsonp(r1.text))
    print("[1] serviceLogin -> code=%s" % step1.get("code"))
    if step1.get("code") != 0:
        dump_masked(step1)
    qs, ssign = step1.get("qs"), step1.get("_sign")

    r2 = sess.post("https://account.xiaomi.com/pass/serviceLoginAuth2", data={
        "user": user,
        "hash": hashlib.md5(password.encode()).hexdigest().upper(),
        "qs": qs, "_sign": ssign,
        "sid": "xiaomiio", "_json": "true",
    }, timeout=15)
    step2 = json.loads(_strip_jsonp(r2.text))
    code = step2.get("code")
    print("[2] serviceLoginAuth2 -> code=%s desc=%s" % (code, step2.get("desc", "")))

    if code != 0:
        dump_masked(step2)
        if code == 700:
            print("    需要验证码：%s" % step2.get("captchaurl"))
        sys.exit(1)

    # ---- A 直连路径 ----
    if step2.get("ssecurity"):
        out = _finish_client_sign(sess, step2)
        if not out["serviceToken"]:
            print("[3] 未拿到 serviceToken，final:", dump_masked(out["final"]))
            sys.exit(1)
        out.pop("final")
        print("[3] 登录成功(A 直连) userId=%s" % out["userId"])
        save_session(out)
        return

    # ---- B 分支：无 ssecurity，先看响应里有什么 ----
    print("[2] 直连未返回 ssecurity，响应字段如下：")
    dump_masked(step2)

    location = step2.get("location")
    if location:
        r = sess.get(location, allow_redirects=True, timeout=20)
        print("[3] GET location -> %s" % r.url)
        try:
            print("    body: %s" % json.dumps(json.loads(_strip_jsonp(r.text)),
                                              ensure_ascii=False)[:300])
        except ValueError:
            print("    body(前200字符): %s" % r.text[:200])

    pass_token = (step2.get("passToken") or sess.cookies.get("passToken"))
    if pass_token:
        print("[4] 用 passToken 重放 serviceLogin ...")
        out, step4 = _pass_token_login(sess, pass_token, step2.get("userId"), server)
        if out and out.get("serviceToken"):
            out.pop("final", None)
            print("[5] 登录成功(B 通知路径) userId=%s" % out["userId"])
            save_session(out)
            return
        print("[5] passToken 路径未拿到 ssecurity：")
        dump_masked(step4 or {})
        sys.exit(1)

    print("需要人工介入：请把上面的响应字段（已掩码）贴回来分析")
    sys.exit(1)


# --------------------------------------------------------------- signed api -
def api(session: dict, path: str, params: dict) -> dict:
    """POST https://api.io.mi.com/app<path> —— Crypto.RC4 签名请求（与 micloud/miutils 一致）。"""
    nonce = gen_nonce()
    sec = session_security(session["ssecurity"], nonce)
    method, url = "POST", "/app" + path

    cipher = new_rc4_cipher(sec)
    plain = json.dumps(params, separators=(",", ":"))

    rc4_hash = sign(method, path, {"data": plain}, sec)          # 明文 hash
    enc_data = base64.b64encode(cipher.crypt(plain.encode("utf-8"))).decode()
    enc_hash = base64.b64encode(cipher.crypt(rc4_hash.encode("utf-8"))).decode()
    signature = sign(method, path, {"data": enc_data, "rc4_hash__": enc_hash}, sec)

    form = {
        "data": enc_data,
        "rc4_hash__": enc_hash,
        "signature": signature,
        "_nonce": nonce,
        "ssecurity": session["ssecurity"],
    }
    cookies = {"userId": session["userId"], "serviceToken": session["serviceToken"],
               **session.get("cookies", {})}
    headers = {"User-Agent": UA,
               "Content-Type": "application/x-www-form-urlencoded"}
    r = requests.post("https://api.io.mi.com" + url, data=form,
                      cookies=cookies, headers=headers, timeout=20)
    try:
        dec = decrypt_response(sec, r.text)
        return json.loads(dec)
    except (ValueError, Exception):
        return {"http_status": r.status_code, "text": r.text[:800]}


# ------------------------------------------------------------------ cmds ----
def cmd_devices():
    s = load_session()
    body = {"start_did": "0", "limit": "200", "get_spec": True,
            "get_cariot_device": True, "dids": []}
    resp = api(s, "/v2/home/device_list_page", body)
    if "result" not in resp:
        print("响应顶层键:", list(resp.keys()))
        print("响应[:1200]:", json.dumps(resp, ensure_ascii=False)[:1200])
        return
    for d in resp["result"]["list"]:
        print("%s  %-28s %s  type=%s" % (d.get("did"), d.get("model"),
                                         d.get("name"), d.get("type")))
        if d.get("isOnline") == 0:
            print("     (offline)")


def cmd_switch(did: str, onoff: str):
    s = load_session()
    value = True if onoff in ("on", "1", "true", "开") else False
    body = {"id": 1, "method": "set_properties", "params":
            [{"did": did, "siid": 2, "piid": 1, "value": value}]}
    print("rpcv2:", json.dumps(body, ensure_ascii=False))
    print(json.dumps(api(s, "/home/rpcv2/" + did, body), ensure_ascii=False, indent=1))
    time.sleep(1.0)
    body2 = {"id": 2, "method": "get_properties", "params":
             [{"did": did, "siid": 2, "piid": 1}]}
    print("查询:", json.dumps(api(s, "/home/rpcv2/" + did, body2), ensure_ascii=False))


def cmd_rpcv2(did: str, method_json: str):
    s = load_session()
    body = {"id": 1, "method": json.loads(method_json).get("method"),
            "params": json.loads(method_json).get("params")}
    print(json.dumps(api(s, "/home/rpcv2/" + did, body), ensure_ascii=False, indent=1))


def cmd_rpc(did: str, json_str: str):
    s = load_session()
    body = json.loads(json_str)
    print(json.dumps(api(s, "/home/rpc/" + did, body), ensure_ascii=False, indent=1))


def cmd_raw(path: str, json_str: str):
    s = load_session()
    print(json.dumps(api(s, path, json.loads(json_str)),
                     ensure_ascii=False, indent=1)[:4000])


def main():
    cmds = {"login": (login, 2), "devices": (cmd_devices, 0), "switch": (cmd_switch, 2),
            "rpcv2": (cmd_rpcv2, 2), "rpc": (cmd_rpc, 2), "raw": (cmd_raw, 2)}
    if len(sys.argv) < 2 or sys.argv[1] not in cmds:
        print(__doc__)
        sys.exit(1)
    fn, nargs = cmds[sys.argv[1]]
    fn(*sys.argv[2:2 + nargs])


if __name__ == "__main__":
    main()
