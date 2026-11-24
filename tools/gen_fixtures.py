# -*- coding: utf-8 -*-
"""
生成 mijia-core Rust 对拍夹具 vectors.json
==========================================

调用 mijia_proto 的加密原语（协议金标准），用固定输入生成确定性输入输出对。
Rust 侧 tests/vectors.rs 读取本文件做字节级对拍。

用法：python tools/gen_fixtures.py
输出：mijia-core/fixtures/vectors.json
"""
import base64
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import mijia_proto as mp  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "mijia-core", "fixtures", "vectors.json")


def b64e(b: bytes) -> str:
    return base64.b64encode(b).decode()


# 固定测试密钥材料（确定性，非真实凭据）
SSEC16 = b64e(bytes((i * 17 + 5) & 255 for i in range(16)))   # 16B ssecurity（真实形态）
SSEC16B = b64e(bytes((i * 31 + 99) & 255 for i in range(16)))  # 第二把
NONCE12 = b64e(bytes(range(1, 13)))                            # 12B nonce（8B 随机 + 4B 分钟）
NONCE12B = b64e(bytes((255 - i) & 255 for i in range(12)))
KEY32 = bytes((i * 7 + 11) & 255 for i in range(32))           # sessionSecurity 解码后的形态


def rc4_section():
    """标准 RC4 单发加密：经典向量 + 二进制 key。"""
    cases = [
        {"key_b64": b64e(b"Key"), "data_b64": b64e(b"Plaintext"),
         "note": "classic-1"},
        {"key_b64": b64e(b"Wiki"), "data_b64": b64e(b"pedia"), "note": "classic-2"},
        {"key_b64": b64e(b"Secret"), "data_b64": b64e(b"Attack at dawn"),
         "note": "classic-3"},
        {"key_b64": b64e(KEY32), "data_b64": b64e(bytes(range(64))), "note": "binary-key"},
        {"key_b64": b64e(base64.b64decode(SSEC16)), "data_b64": b64e(b"A" * 40),
         "note": "ssec-derived"},
    ]
    for c in cases:
        c["out_b64"] = b64e(mp.rc4(base64.b64decode(c["key_b64"]),
                                   base64.b64decode(c["data_b64"])))
    return cases


def rc4_stream_section():
    """带状态 RC4Stream：同一 keystream 连续 crypt（复刻请求加密的调用序列）。"""
    cases = []
    for name, key in (("sec-32b", KEY32), ("ssec-16b", base64.b64decode(SSEC16))):
        c = mp.RC4Stream(key)
        steps = []
        drop = c.crypt(bytes(1024))          # g7c：构造后 drop 1024
        steps.append({"in_b64": b64e(bytes(1024)), "out_b64": b64e(drop)})
        chunk1 = c.crypt(b'{"id":1,"method":"set_properties"}')
        steps.append({"in_b64": b64e(chunk1), "out_b64": None})
        chunk2 = c.crypt(b"POST&/home/rpcv2/1&data=xx&sec")
        steps.append({"in_b64": b64e(chunk2), "out_b64": None})
        chunk3 = c.crypt(bytes(range(48)))
        steps.append({"in_b64": b64e(chunk3), "out_b64": None})
        cases.append({"key_b64": b64e(key), "steps": steps})
    return cases


def nonce_section():
    """gen_nonce 公式：b64( 8B 随机 + int32_be(分钟) )。"""
    cases = []
    for rnd8, minute in (
        (bytes(range(8)), 29283182),
        (bytes((i * 13 + 1) & 255 for i in range(8)), 0),
        (b"\x00" * 8, -1),                       # 负分钟（时钟回拨）
        (bytes((255 - i) & 255 for i in range(8)), 2147483647),  # i32 上界
    ):
        raw = rnd8 + __import__("struct").pack(">i", minute)
        cases.append({"rnd8_b64": b64e(rnd8), "minute": minute,
                      "expected": b64e(raw)})
    return cases


def session_security_section():
    cases = []
    for ssec, nonce in ((SSEC16, NONCE12), (SSEC16B, NONCE12B),
                        (SSEC16, NONCE12B), (SSEC16B, NONCE12)):
        cases.append({"ssecurity": ssec, "nonce": nonce,
                      "expected": mp.session_security(ssec, nonce)})
    return cases


def sign_section():
    """sign()：METHOD&path&k=v(排序)&sessionSecurity -> b64(SHA1)。"""
    sec = mp.session_security(SSEC16, NONCE12)
    plain_dl = json.dumps({"start_did": "0", "limit": "200", "get_spec": True,
                           "get_cariot_device": True, "dids": []},
                          separators=(",", ":"))
    cases = [
        # 设备列表（真实请求的 rc4_hash 明文签名）
        {"method": "POST", "path": "/v2/home/device_list_page",
         "params": {"data": plain_dl}, "session_security": sec},
        # rpcv2 密文签名（data + rc4_hash__ 两个字段，验证排序 data < rc4_hash__）
        {"method": "POST", "path": "/home/rpcv2/900000001",
         "params": {"data": "ENC_DATA_B64", "rc4_hash__": "ENC_HASH_B64"},
         "session_security": sec},
        # 多 key 排序验证
        {"method": "POST", "path": "/x", "params": {"b": "2", "a": "1", "c": "3"},
         "session_security": sec},
        # 空方法（部分客户端登录收尾用）
        {"method": "", "path": "/y", "params": {"k": "v"}, "session_security": sec},
        # GET 方法大写化
        {"method": "get", "path": "/z", "params": {}, "session_security": sec},
    ]
    for c in cases:
        c["expected"] = mp.sign(c["method"], c["path"], c["params"],
                                c["session_security"])
    return cases


def client_sign_section():
    """clientSign = b64(SHA1("nonce=<十进制>&" + ssecurity))（登录收尾）。"""
    cases = []
    for nonce_dec, ssec in (
        ("1592630680355408512", SSEC16),          # auth2 返回的十进制 nonce 形态
        ("0", SSEC16B),
        ("18446744073709551615", SSEC16),          # u64 上界
    ):
        cases.append({"nonce": nonce_dec, "ssecurity": ssec,
                      "expected": b64e(__import__("hashlib").sha1(
                          ("nonce=" + nonce_dec + "&" + ssec).encode("utf-8")
                      ).digest())})
    return cases


def _encrypt_like_api(ssec, nonce, method, path, params):
    """复刻 mijia_proto.api() 的加密段（不含 HTTP）。"""
    sec = mp.session_security(ssec, nonce)
    plain = json.dumps(params, separators=(",", ":"))
    c = mp.new_rc4_cipher(sec)
    rc4_hash = mp.sign(method, path, {"data": plain}, sec)
    enc_data = b64e(c.crypt(plain.encode("utf-8")))
    enc_hash = b64e(c.crypt(rc4_hash.encode("utf-8")))
    signature = mp.sign(method, path, {"data": enc_data, "rc4_hash__": enc_hash}, sec)
    return {
        "ssecurity": ssec, "nonce": nonce, "method": method, "path": path,
        "params": params, "plain": plain, "session_security": sec,
        "rc4_hash": rc4_hash, "enc_data": enc_data, "enc_hash": enc_hash,
        "signature": signature,
        "form": {"data": enc_data, "rc4_hash__": enc_hash, "signature": signature,
                 "_nonce": nonce, "ssecurity": ssec},
    }


def request_encrypt_section():
    dl = {"start_did": "0", "limit": "200", "get_spec": True,
          "get_cariot_device": True, "dids": []}
    switch_on = {"id": 1, "method": "set_properties",
                 "params": [{"did": "900000001", "siid": 2, "piid": 1, "value": True}]}
    get_props = {"id": 2, "method": "get_properties",
                 "params": [{"did": "900000001", "siid": 2, "piid": 1}]}
    non_ascii = {"id": 3, "method": "set_properties",
                 "params": [{"did": "100", "siid": 3, "piid": 1,
                             "value": "客厅 灯「主」—TTS"}]}   # 强制 \uXXXX 转义路径
    miio = {"id": 1, "method": "miIO.info", "params": []}
    cases = [
        _encrypt_like_api(SSEC16, NONCE12, "POST", "/v2/home/device_list_page", dl),
        _encrypt_like_api(SSEC16, NONCE12, "POST", "/home/rpcv2/900000001", switch_on),
        _encrypt_like_api(SSEC16B, NONCE12B, "POST", "/home/rpcv2/900000001", get_props),
        _encrypt_like_api(SSEC16, NONCE12B, "POST", "/home/rpcv2/100", non_ascii),
        _encrypt_like_api(SSEC16B, NONCE12, "POST", "/home/rpc/900000003", miio),
    ]
    return cases


def response_decrypt_section():
    """decrypt_response：全新 RC4(b64d(sec)) drop1024 → 解密 b64 payload。"""
    samples = [
        '{"code":0,"message":"ok","result":{"list":[{"did":"900000001",'
        '"model":"xiaomi.switch.w1","name":"西窗开关","isOnline":1}]}}',
        '{"id":1,"code":0,"method":"set_properties","exe_time":12.3,'
        '"ret":["ok"],"result":0}',
        '{"id":2,"code":0,"method":"get_properties","result":'
        '[{"did":"900000001","siid":2,"piid":1,"code":0,"value":true}]}',
    ]
    cases = []
    for i, text in enumerate(samples):
        for ssec, nonce in ((SSEC16, NONCE12), (SSEC16B, NONCE12B)):
            sec = mp.session_security(ssec, nonce)
            c = mp.RC4Stream(base64.b64decode(sec))
            c.crypt(bytes(1024))
            payload = b64e(c.crypt(text.encode("utf-8")))
            cases.append({"session_security": sec, "payload_b64": payload,
                          "expected_utf8": text, "note": "case-%d" % i})
    # 二进制解密（非 UTF8，验 hex）
    sec = mp.session_security(SSEC16, NONCE12)
    c = mp.RC4Stream(base64.b64decode(sec))
    c.crypt(bytes(1024))
    raw = bytes((i * 37 + 200) & 255 for i in range(96))
    cases.append({"session_security": sec, "payload_b64": b64e(c.crypt(raw)),
                  "expected_b64": b64e(raw), "note": "binary"})
    return cases


def main():
    vectors = {
        "meta": {
            "generator": "tools/gen_fixtures.py",
            "gold": "mijia_proto.py（实测闭环的 Python 原型）",
            "note": "所有 key/data 为固定测试材料，非真实凭据",
        },
        "rc4": rc4_section(),
        "rc4_stream": rc4_stream_section(),
        "nonce": nonce_section(),
        "session_security": session_security_section(),
        "sign": sign_section(),
        "client_sign": client_sign_section(),
        "request_encrypt": request_encrypt_section(),
        "response_decrypt": response_decrypt_section(),
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(vectors, f, ensure_ascii=True, indent=1)
    print("已生成", OUT)
    for k, v in vectors.items():
        if k != "meta":
            print("  %-18s %d cases" % (k, len(v)))


if __name__ == "__main__":
    main()
