# -*- coding: utf-8 -*-
"""把集成测试夹具从 _work/（含真实账号数据、不入库）搬进 mijia-core/fixtures/（公开数据、入库）。

动机（见 _work/卡片上线标准设计.md 第三节）：
`mijia-core/tests/card_engine.rs` 一直从 `../_work/` 读夹具，而 `_work` 里全是真实
did / 设备名 / 家庭门牌地址 / 抓包，绝不能进公开仓库；一旦被 .gitignore 忽略，
新克隆的仓库就跑不了测试。本脚本做**去账号化**搬家：

1. 三份 getappconfig 模板配置、miot-spec instance 样本 → 原样拷（公开注册表数据，无 PII）；
2. `spec_classify.json`（按真实 did 索引，含用户自取设备名）→ 重建成**按 model 索引**、
   只保留 `CardSpec::from_classify` 真正消费的 `stats.*`，丢弃 name/online/did；
3. 输出一份 did→model 对照（只打印，不落盘），供改写测试调用点。

用法：python tools/make_card_fixtures.py
"""
import json
import os
import shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.path.join(ROOT, "_work")
FIX = os.path.join(ROOT, "mijia-core", "fixtures")
SPECS = os.path.join(FIX, "specs")
TEMPLATES = os.path.join(FIX, "templates")

# 公开数据：无签名 GET 拿到的模板 + miot-spec.org 的 instance 样本
COPY_MAP = [
    ("card_config_card_control_miotspec_config.json", "templates"),
    ("card_config_card_control_config.json", "templates"),
    ("card_config_card_default_txt.json", "templates"),
    ("spec_aircondition_m4.json", "specs"),
]


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def main():
    os.makedirs(SPECS, exist_ok=True)
    os.makedirs(TEMPLATES, exist_ok=True)

    for name, bucket in COPY_MAP:
        src = os.path.join(WORK, name)
        dst = os.path.join(TEMPLATES if bucket == "templates" else SPECS, name)
        if not os.path.exists(src):
            print("[缺] %s（跳过）" % src)
            continue
        shutil.copyfile(src, dst)
        print("[拷] %s -> %s (%d B)" % (name, os.path.relpath(dst, ROOT), os.path.getsize(dst)))

    classify_src = os.path.join(WORK, "spec_classify.json")
    raw = load(classify_src)
    by_model = {}
    mapping = []
    dropped = set()
    for did, dev in raw.items():
        model = dev.get("model") or ""
        if not model:
            dropped.add(did)
            continue
        # 只留 from_classify 消费的 stats；其余（真实 did、用户自取设备名、在线态）一律不进仓库
        stats = dev.get("stats") or {}
        entry = {
            "model": model,
            "stats": {
                k: stats.get(k, []) for k in ("observe", "control", "meta", "notify_only")
            },
        }
        if model in by_model:
            print("[警告] model 重复 %s（did=%s），后者覆盖前者——需要人工合并" % (model, did))
        by_model[model] = entry
        mapping.append((did, model, dev.get("name", "")))

    out = os.path.join(FIX, "specs_by_model.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(by_model, f, ensure_ascii=False, indent=1, sort_keys=True)
    print("[建] %s：%d 个型号（按 model 索引，无 did / 无用户设备名）"
          % (os.path.relpath(out, ROOT), len(by_model)))

    print("\n=== 自检 ===")
    # 1) 结构检查：顶层键必须都是 model 形态，且不得残留用户字段
    did_like = [m for m in by_model if m.startswith(("blt.", "miwifi.", "lumi.158d"))
                or "/" in m]
    print("  顶层键疑似 did 形态: %s" % (did_like or "无"))
    bad_fields = []
    for m, e in by_model.items():
        bad_fields += ["%s.%s" % (m, k) for k in ("name", "online", "did", "token", "mac")
                       if k in e]
    print("  残留用户字段: %s" % (bad_fields or "无"))

    # 2) 用户自取设备名扫描：落在官方 spec 文本（desc/comment/description）里的不算泄漏，
    #    否则就是真泄漏。「网关」这类高频通用词尤其容易在官方描述里撞车，哭错狼的
    #    检查等于没有检查，所以按出现位置区分。
    PUBLIC_KEYS = {"desc", "comment", "description"}

    def occurrences(node, name, path=""):
        hits = []
        if isinstance(node, dict):
            for k, v in node.items():
                hits += occurrences(v, name, path + "/" + k)
        elif isinstance(node, list):
            for i, v in enumerate(node):
                hits += occurrences(v, name, path + "[%d]" % i)
        elif isinstance(node, str) and name in node:
            hits.append(path)
        return hits

    leaks, public_hits = [], []
    for _d, model, name in mapping:
        if not name:
            continue
        for path in occurrences(by_model.get(model, {}), name):
            leaf = path.rsplit("/", 1)[-1].split("[")[0]
            (public_hits if leaf in PUBLIC_KEYS else leaks).append("%s <- %s" % (name, path))
    print("  真泄漏(用户字段里的设备名): %s" % (leaks or "无"))
    if public_hits:
        print("  仅出现在官方 spec 文本中(公开数据，不算泄漏，留给人工复核):")
        for h in public_hits[:6]:
            print("    " + h)

    print("\n=== did → model 对照（只打印，不落盘；用于改写测试调用点）===")
    for did, model, name in sorted(mapping, key=lambda x: x[1]):
        print('  "%s" -> "%s"   # 用户自取名字：%s' % (did, model, name))
    if dropped:
        print("[跳过] 无 model 的条目：%s" % sorted(dropped))


if __name__ == "__main__":
    main()
