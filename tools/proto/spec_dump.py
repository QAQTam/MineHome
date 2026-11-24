# -*- coding: utf-8 -*-
"""能力盘点（只读）：设备 spec_type -> miot-spec.org 实例定义 -> 可写属性枚举。
不向任何设备下发控制请求。
用法：python spec_dump.py
"""
import json
import urllib.parse
import urllib.request

import mijia_proto as mp


def http_get_json(url):
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read().decode("utf-8"))


def urn_name(urn):
    parts = urn.split(":")
    return parts[3] if len(parts) >= 4 else urn


def main():
    s = mp.load_session()
    body = {"start_did": "0", "limit": "200", "get_spec": True,
            "get_cariot_device": True, "dids": []}
    resp = mp.api(s, "/v2/home/device_list_page", body)
    for d in resp["result"]["list"]:
        online = "online" if d.get("isOnline") else "offline"
        print("\n=== %s | %s | did=%s | %s ===" % (
            d.get("name"), d.get("model"), d.get("did"), online))
        st = d.get("spec_type", "")
        if not st:
            print("  (无 spec_type)")
            continue
        try:
            inst = http_get_json("https://miot-spec.org/miot-spec-v2/instance?type=" + st)
        except Exception as e:
            print("  spec 拉取失败: %s" % e)
            continue
        for svc in inst.get("services", []):
            sname = urn_name(svc.get("type", ""))
            for p in svc.get("properties", []):
                acc = p.get("access", [])
                if "write" not in acc and "writeonly" not in acc:
                    continue
                pname = urn_name(p.get("type", ""))
                extra = ""
                if p.get("value-range"):
                    extra += " range=%s" % p["value-range"]
                if p.get("value-list"):
                    extra += " list=%s" % p["value-list"]
                print("  s%s/p%s %s.%s [%s]%s" % (
                    svc.get("iid"), p.get("iid"), sname, pname,
                    ",".join(acc), extra))


if __name__ == "__main__":
    main()
