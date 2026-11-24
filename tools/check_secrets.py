# -*- coding: utf-8 -*-
"""扫描"会进公开仓库的文件"里残留的真实账号痕迹。开放外部 PR 的第一道闸门。

判定清单不写死，全部从 _work 的真实产物里现取（换账号不用改代码）：
  - 真实 did：_work/spec_classify.json 的键
  - 真实 uid：_work/spec_classify.json 值里的 uid，或 session.json 的 userId
  - 用户自取设备名（可能暴露住址/习惯，如「书桌灯」）
  - 家庭门牌地址与经纬度（_work 里 gethome 响应的 address/latitude/longitude）
  - 凭据形态：passToken/serviceToken/ssecurity 的值、32+ 位 hex、长 base64
  - 手机号形态

被 .gitignore 忽略的路径一律不扫（那些本来就不入库），扫的是 `git status -uall`
的待纳管清单，所以这个脚本的结论**等于**"别人 clone 到的一刻会看到什么"。

用法：
  python tools/check_secrets.py            # 扫全部待纳管文件
  python tools/check_secrets.py --list     # 只列被扫的文件数
  python tools/check_secrets.py --all      # 扫全仓库已跟踪文件（CI 用）
退出码：0=干净，1=发现问题（CI 直接用）。
"""
import json
import os
import re
import subprocess
import sys
import collections

ROOT = os.path.dirname(os.path.abspath(__file__ + "/.."))
WORK = os.path.join(ROOT, "_work")

# 这些后缀才值得逐行读；其余（图片/二进制/归档）只做文件名级检查
TEXT_EXT = {".rs", ".ets", ".py", ".md", ".json", ".json5", ".toml", ".txt",
            ".ts", ".js", ".yaml", ".yml", ".sh", ".ps1", ".bat", ".c", ".h"}


def git_tracked_and_untracked():
    """待纳管文件清单（含未跟踪、已按 .gitignore 过滤）。"""
    if "--all" in sys.argv:
        # CI 场景：干净 checkout 没有待提交项，`git status` 会扫到 0 个文件、
        # 闸门形同虚设——必须退回扫**全量已跟踪文件**。
        out = subprocess.run(
            ["git", "-c", "core.quotepath=false", "ls-files", "-z"],
            cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace"
        )
        if out.returncode != 0:
            sys.exit("git ls-files 失败: %s" % out.stderr.strip())
        return [f for f in out.stdout.split("\0")
                if f and os.path.isfile(os.path.join(ROOT, f.replace("/", os.sep)))]
    # core.quotepath=false：中文/非 ASCII 文件名在 git status 里默认被八进制转义
    # 并加引号，早期版本因此**整类漏扫了 docs 里的中文文档**（2026-11-24 修复）。
    out = subprocess.run(
        ["git", "-c", "core.quotepath=false", "status", "--porcelain", "-uall", "--ignored=no"],
        cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace"
    )
    if out.returncode != 0:
        sys.exit("git status 失败（仓库还没 init？）: %s" % out.stderr.strip())
    files = []
    for line in out.stdout.splitlines():
        path = line[3:].strip()
        if "->" in path:
            path = path.split("->")[-1].strip()
        path = path.strip('"')
        full = os.path.join(ROOT, path.replace("/", os.sep))
        if os.path.isfile(full):
            files.append(path)
    return files


def build_needles():
    """从真实产物里取要拦的字串。缺文件就跳过该项（不硬失败，方便 CI 早跑）。"""
    names, values = [], set()

    classify = os.path.join(WORK, "spec_classify.json")
    if os.path.exists(classify):
        d = json.load(open(classify, encoding="utf-8"))
        for did, dev in d.items():
            values.add(str(did))
            nm = (dev or {}).get("name")
            if nm:
                names.append(str(nm))
            uid = (dev or {}).get("uid")
            if uid:
                values.add(str(uid))

    sess = os.path.join(ROOT, "session.json")
    if os.path.exists(sess):
        try:
            s = json.load(open(sess, encoding="utf-8"))
            for k in ("userId", "passToken", "serviceToken", "ssecurity"):
                v = s.get(k)
                if isinstance(v, str) and len(v) >= 6:
                    values.add(v)
        except Exception as e:  # noqa: BLE001
            print("  [警告] session.json 解析失败，跳过该项：%s" % e)

    # 门牌地址/经纬度：从房间探测留档里取
    for f in ("devlist_raw.json",):
        p = os.path.join(WORK, f)
        if os.path.exists(p):
            t = open(p, encoding="utf-8", errors="replace").read()
            for m in re.finditer(r'"address"\s*:\s*"([^"]{6,})"', t):
                names.append(m.group(1))
    return sorted(values), sorted(set(names))


# 形态类规则。踩过的坑：`[A-Za-z0-9+/]{40,}` 会把「代码路径」「integrity 哈希」
# 「用斜杠连接的标识符列表」全报成凭据（一次报出 842 条，绝大多数是噪声）。
# 所以：候选里不许出现斜杠；且必须同时含大小写与数字（凭据的典型形态）。
B64_OK = re.compile(r"^(?=.*[a-z])(?=.*[A-Z])(?=.*\d)[A-Za-z0-9+=]{40,}$")
PATTERNS = [
    # 必须是**引号里的长字面量**才算凭据；`ssecurity = jstr_opt(v)` 这类变量赋值
    # 曾被误报（右值没引号、也不是秘密值）。
    ("凭据字段赋值（带字面量）", re.compile(
        r"(passToken|serviceToken|ssecurity|beaconkey|deviceIdKey)\s*[:=]\s*[\"'][A-Za-z0-9+/=_-]{12,}[\"']",
        re.I)),
    ("手机号", re.compile(r"\b1[3-9]\d{9}\b")),
    ("经纬度对（住址线索）", re.compile(r"\b(1[0-9]{2}\.\d{4,})\b")),
]

# 只对"值"形态做规则匹配；这两类文件里的长串是完整性哈希，不是凭据
INTEGRITY_FILES = ("oh-package-lock.json5",)

# 显式豁免表：命中**路径前缀**（也支持 `路径:行号` 精确到行）+ 必须写理由。
# 目的是让扫描器可用而不是"报一堆就没人看"——但豁免一律留痕，reviewer 看得见，
# 而且每一条都对应一个真实取舍，不是为了让扫描变绿而关眼。
ALLOW = [
    # MiBeacon 真实广播样本：换成合成帧就失去与真机对拍的价值（bindkey 本身是合成的）
    ("mijia-core/src/ble.rs",
     "真机抓取的 MiBeacon 广播帧回归向量（含设备 id 字段；密钥参数是 001122… 合成值）"),
    # ble_auth.rs 的 golden 向量：token/随机数是 Python 侧硬编码的假数据
    # （aabbcc…/001122…/ffeedd…），okm/confirm/帧全部由假数据派生，非真实凭据
    ("mijia-core/src/ble_auth.rs",
     "合成 golden 向量（Python pycryptodome 由哑 token/随机数派生），对拍 HKDF/CCM/spec 解析"),
    ("mijia-core/fixtures/vectors.json",
     "tools/gen_fixtures.py 生成的确定性合成密钥材料（bytes(range(64)) 之类），非真实凭据"),
    ("app/oh-package-lock.json5",
     "包完整性哈希"),
    # profile.rs 的负向断言样例词：这条测试在**保证**档案里不出现真实设备名，
    # 换掉这个词反而削弱测试。只留这一行。
    ("mijia-core/src/profile.rs:494",
     "单测「档案里不得出现账号字段」的负向断言样例词（通用词「书桌灯」），非真实数据"),
]


def allowed(rel, ln):
    # 豁免项既支持目录/文件前缀，也支持 `路径:行号` 精确到一行
    # （用正则判定，而不是对某个行号写特例）
    for prefix, reason in ALLOW:
        m = re.fullmatch(r"(.*?):(\d+)", prefix)
        if m:
            if rel == m.group(1) and ln == int(m.group(2)):
                return reason
            continue
        if rel.startswith(prefix) or rel == prefix:
            return reason
    return None


def shape_hits(line, rel):
    """形态类命中：按 token 切开后逐个判，避免整行匹配到路径。"""
    out = []
    for tok in re.split(r"[^A-Za-z0-9+=/]+", line):
        if len(tok) < 32:
            continue
        if "/" in tok or "\\" in tok:
            continue  # 路径 / integrity 哈希
        if re.fullmatch(r"[0-9a-f]{32,}", tok):
            # 纯 hex：只有 40+ 位才像凭据（32 位常见于各种 id/摘要）
            if len(tok) >= 40:
                out.append("40+ 位 hex（凭据/摘要形态）：%s" % tok[:48])
            continue
        if len(tok) >= 40 and B64_OK.match(tok):
            out.append("长 base64（凭据形态）：%s" % tok[:48])
    return out


def main():
    only_list = "--list" in sys.argv
    values, names = build_needles()
    files = git_tracked_and_untracked()
    print("待纳管文件 %d 个；动态清单：真实 id/凭据 %d 条、用户设备名与地址 %d 条"
          % (len(files), len(values), len(names)))
    if only_list:
        for f in files[:20]:
            print("  " + f)
        return 0

    findings = []
    scanned = 0
    for rel in files:
        ext = os.path.splitext(rel)[1].lower()
        if ext not in TEXT_EXT:
            continue
        # 不扫自己：本文件的正则源码里就含手机号/hex 形态，扫出来永远是假警报
        if rel.endswith("tools/check_secrets.py"):
            continue
        scanned += 1
        path = os.path.join(ROOT, rel.replace("/", os.sep))
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError as e:
            findings.append((rel, 0, "读取失败 %s" % e))
            continue
        skip_shape = rel.endswith(INTEGRITY_FILES)
        for lineno, line in enumerate(text.splitlines(), 1):
            for v in values:
                if v and v in line:
                    findings.append((rel, lineno, "真实 id/凭据 %s" % v))
            for n in names:
                # 设备名可能是常见词（「网关」「电脑」），太短的不算证据
                if len(n) >= 3 and n in line:
                    findings.append((rel, lineno, "用户设备名/地址 %s" % n))
            for label, pat in PATTERNS:
                m = pat.search(line)
                if m:
                    findings.append((rel, lineno, "%s：%s" % (label, m.group(0)[:48])))
            if not skip_shape:
                for h in shape_hits(line, rel):
                    findings.append((rel, lineno, h))

    # 明显是测试用的合成值不计入失败
    synthetic = re.compile(r"synthetic|0000000|123456|MDEyMzQ1Njc4OQ")
    exempted = collections.Counter()
    real = []
    for rel, ln, why in findings:
        if synthetic.search(why + " " + (rel or "")):
            exempted["（自动判定为合成值）"] += 1
            continue
        reason = allowed(rel, ln)
        if reason:
            exempted["%s —— %s" % (rel, reason)] += 1
            continue
        real.append((rel, ln, why))

    print("逐行扫描文本文件 %d 个" % scanned)
    if exempted:
        print("已豁免 %d 条（每条都有写明的理由，review 时请核对理由是否成立）："
              % sum(exempted.values()))
        for k, n in exempted.most_common():
            print("   %4d  %s" % (n, k))
    if not real:
        print("✅ 豁免表之外未发现真实账号痕迹")
        return 0
    per_file = collections.Counter(rel for rel, _ln, _w in real)
    per_kind = collections.Counter(w.split("：")[0].split(" ")[0] for _r, _l, w in real)
    print("❌ 命中 %d 条 / %d 个文件" % (len(real), len(per_file)))
    print("   按类型：" + ", ".join("%s=%d" % (k, n) for k, n in per_kind.most_common()))
    print("   按文件：")
    for rel, n in per_file.most_common():
        print("     %4d  %s" % (n, rel))
    print("   明细（最多 40 条）：")
    for rel, ln, why in real[:40]:
        print("     %s:%d  %s" % (rel, ln, why))
    return 1


if __name__ == "__main__":
    sys.exit(main())
