#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""MineHome 开发环境自检（新机器第一步先跑这个）。

零第三方依赖（只用标准库），Windows / Linux / macOS 通用。
检查分级：
  fail = 缺了连本仓库的测试闸门都跑不动（git/cargo/python/仓库完整性）
  warn = 影响某一类构建（ohrs 原生库、hvigor HAP、pip 工具脚本），可以先放着
每项都给「装什么 + 怎么装」的提示；--json 输出给 CI 和 AI agent 消费。

用法：
  python tools/check_env.py            # 人读表格
  python tools/check_env.py --json     # 机器可读（agent / CI）
  just check                           # 同义快捷入口（需要 just，可选）
"""
import json
import os
import platform
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(os.path.join(__file__, "..")))


def ver_tuple(s):
    m = re.search(r"(\d+)\.(\d+)(?:\.(\d+))?", s or "")
    if not m:
        return None
    return tuple(int(x) if x else 0 for x in m.groups()[:2])


def run(cmd, timeout=15):
    """跑命令拿 (rc, stdout)。找不到/超时返回 None。"""
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                           encoding="utf-8", errors="replace", shell=False)
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except Exception:  # noqa: BLE001
        return None


def probe_version(binary, args):
    exe = shutil.which(binary)
    if not exe:
        return None, None
    r = run([exe] + args)
    if not r or r[0] != 0:
        return exe, None
    return exe, r[1].strip().splitlines()[0] if r[1].strip() else ""


class Check:
    def __init__(self, items):
        self.items = items

    def add(self, cid, tier, status, name, version=None, path=None,
            detail="", hint=""):
        self.items.append({"id": cid, "tier": tier, "status": status,
                           "name": name, "version": version, "path": path,
                           "detail": detail, "hint": hint})


def check_binary(ck, cid, tier, name, argv, vmin=None, opt=False, hint=""):
    exe, verline = probe_version(argv[0], argv[1:])
    if not exe:
        ck.add(cid, tier, "warn" if opt else "fail", name,
               detail="不在 PATH", hint=hint)
        return
    v = ver_tuple(verline)
    if vmin and (v is None or v < vmin):
        ck.add(cid, tier, "warn", name, version=verline, path=exe,
               detail="版本低于建议值 %s" % ".".join(map(str, vmin)), hint=hint)
    else:
        ck.add(cid, tier, "pass", name, version=verline, path=exe,
               detail="OK", hint=hint)


def find_llvm_strip():
    exe = shutil.which("llvm-strip") or shutil.which("llvm-strip.exe")
    if exe:
        return exe
    # DevEco CLT / OHOS SDK 的固定相对位置（本机实测结构）
    bases = [os.environ.get("OHOS_NDK_HOME"),
             os.environ.get("DEVECO_SDK_HOME", "") and
             os.path.join(os.environ["DEVECO_SDK_HOME"], "default", "openharmony"),
             os.environ.get("DEVECO_CLI_CLT_PATH") and
             os.path.join(os.environ["DEVECO_CLI_CLT_PATH"], "sdk", "default",
                          "openharmony")]
    for b in bases:
        if not b:
            continue
        for cand in (os.path.join(b, "native", "llvm", "bin"), b):
            for f in ("llvm-strip.exe", "llvm-strip"):
                p = os.path.join(cand, f)
                if os.path.isfile(p):
                    return p
    return None


def main():
    as_json = "--json" in sys.argv
    items = []
    ck = Check(items)

    # ---------- 基础闸门：缺了连 cargo test / check_secrets 都跑不动 ----------
    check_binary(ck, "git", "base", "git", ["git", "--version"], (2, 30),
                 hint="版本管理；check_secrets --all 也依赖它")
    check_binary(ck, "cargo", "base", "cargo", ["cargo", "--version"], (1, 75),
                 hint="rustup 安装（edition=2021 需 ≥1.75）")
    r = run([sys.executable, "--version"])
    pv = ver_tuple(r[1]) if r else None
    ck.add("python", "base", "pass" if pv and pv >= (3, 9) else "fail",
           "python3", version=r[1].strip() if r else None, path=sys.executable,
           detail="OK" if pv and pv >= (3, 9) else "需要 ≥3.9（仅标准库）",
           hint="python tools/check_secrets.py 与 gen_fixtures.py 的运行时")
    # 链接能力实测：Windows 要 MSVC link.exe（rustc 自己会经 vswhere 定位，
    # PATH 里有与否无关），Linux/macOS 要 cc。唯一可信的判据 = 真编译一个
    # hello 并链接成功（~2s），比查 PATH 可靠。
    rc = run(["rustc", "-vV"])
    if shutil.which("rustc"):
        import tempfile
        probe = os.path.join(tempfile.gettempdir(), "minehome_env_probe.rs")
        out = os.path.join(tempfile.gettempdir(),
                           "minehome_env_probe" + (".exe" if sys.platform == "win32" else ""))
        with open(probe, "w", encoding="utf-8") as f:
            f.write('fn main() { println!("ok"); }\n')
        r = run(["rustc", probe, "-o", out], timeout=120)
        linked = bool(r and r[0] == 0)
        if not linked:
            why = (r[1][:120].replace("\n", " ") if r else "rustc 无响应")
            hint = ("装 Visual Studio Build Tools（C++ 桌面开发工作负载）"
                    if sys.platform == "win32" else "apt install build-essential / xcode-select --install")
            ck.add("linker", "base", "fail", "rustc 链接器",
                   detail="hello 编译链接失败：" + why, hint=hint)
        else:
            ck.add("linker", "base", "pass", "rustc 链接器",
                   detail="hello 编译+链接 OK（host: %s）" % (
                       re.search(r"host: (\S+)", rc[1]).group(1) if rc and rc[1] else "?"))
        for p in (probe, out):
            try:
                os.remove(p)
            except OSError:
                pass
    else:
        ck.add("linker", "base", "fail", "rustc", detail="rustc 不在 PATH",
               hint="rustup 安装")

    # ---------- 仓库完整性（clone 残缺的兜底发现） ----------
    must = ["mijia-core/Cargo.toml", "home-control-mcp/Cargo.toml",
            "mijia-napi/Cargo.toml", "app/entry/oh-package.json5",
            "app/native/libmijia_napi/index.d.ts",
            "mijia-core/fixtures/vectors.json",
            "tools/check_secrets.py", "docs/FORK-SETUP.md"]
    missing = [f for f in must if not os.path.isfile(os.path.join(ROOT, f))]
    ck.add("repo", "base", "fail" if missing else "pass", "仓库完整性",
           detail=("缺: " + ", ".join(missing)) if missing else "%d 个关键文件齐全" % len(must),
           hint="git clone 不完整？重新 clone")

    # ---------- 原生库构建（mijia-napi .so） ----------
    check_binary(ck, "ohrs", "native", "ohrs", ["ohrs", "-V"], opt=True,
                 hint="cargo install --version 1.5.0 ohrs（或读 docs/FORK-SETUP.md §2）")
    ls = find_llvm_strip()
    ck.add("llvm-strip", "native", "pass" if ls else "warn", "llvm-strip",
           path=ls, detail="OK" if ls else "找不到（ohrs 产物 78MB 不 strip 会把 HAP 撑爆）",
           hint="随 DevEco CLT 的 OpenHarmony native SDK 提供，见 docs/FORK-SETUP.md §2")
    ndk = os.environ.get("OHOS_NDK_HOME")
    ck.add("OHOS_NDK_HOME", "native", "pass" if ndk and os.path.isdir(ndk) else "warn",
           "环境变量 OHOS_NDK_HOME", detail=ndk or "未设置（ohrs build 必需）",
           hint="指向 <CLT>/sdk/default/openharmony，export 写法见 --json hint")

    # ---------- HAP 打包与装机 ----------
    check_binary(ck, "node", "hap", "node", ["node", "--version"], (18, 0), opt=True,
                 hint="DevEco CLT 自带 tool/node，或官网 LTS")
    check_binary(ck, "hvigorw", "hap", "hvigorw", ["hvigorw", "--version"], opt=True,
                 hint="DevEco Command-Line Tools（CLT）bin/，见 docs/FORK-SETUP.md")
    check_binary(ck, "ohpm", "hap", "ohpm", ["ohpm", "--version"], opt=True,
                 hint="DevEco CLT 自带")
    check_binary(ck, "hdc", "hap", "hdc", ["hdc", "-v"], opt=True,
                 hint="CLT sdk toolchains/ 目录（PATH 常常不生效，建议全路径）")
    for var in ("DEVECO_SDK_HOME", "HOS_SDK_HOME"):
        v = os.environ.get(var)
        ck.add(var, "hap", "pass" if v else "warn", "环境变量 " + var,
               detail=v or "未设置（hvigor/ohrs 定位 SDK 用）",
               hint="export 示例见 docs/FORK-SETUP.md 开头")

    # ---------- 工具脚本与锦上添花（全部可缺） ----------
    for mod, pkg, why in (("requests", "requests", "tools/gen_fixtures.py 对拍生成"),
                          ("Crypto.Cipher", "pycryptodome", "gen_fixtures / proto 原型的 AES"),
                          ("playwright", "playwright", "tools/proto/login_browser.py 扫码换会话")):
        try:
            __import__(mod)
            ck.add(mod, "optional", "pass", "pip " + pkg, detail="OK")
        except Exception:  # noqa: BLE001
            ck.add(mod, "optional", "warn", "pip " + pkg,
                   detail="未安装（影响：" + why + "）",
                   hint="pip install " + pkg)
    check_binary(ck, "just", "optional", "just", ["just", "--version"], opt=True,
                 hint="任务入口；没有就用 python tools/check_env.py 直接跑")
    check_binary(ck, "gh", "optional", "gh CLI", ["gh", "--version"], opt=True,
                 hint="发布/PR 用；开发不需要")

    n = {"pass": 0, "warn": 0, "fail": 0}
    for it in items:
        n[it["status"]] += 1
    out = {"generated_at": __import__("datetime").datetime.now().astimezone().isoformat(timespec="seconds"),
           "os": platform.system().lower(), "arch": platform.machine().lower(),
           "summary": n, "items": items,
           "verdict": "ENV_OK" if n["fail"] == 0 else "ENV_INCOMPLETE"}
    if as_json:
        print(json.dumps(out, ensure_ascii=False, indent=1))
    else:
        print("MineHome 环境自检 · %s/%s · 检查 %d 项" % (platform.system(), platform.machine(), len(items)))
        icon = {"pass": "✓", "warn": "!", "fail": "x"}
        for tier in ("base", "native", "hap", "optional"):
            rows = [i for i in items if i["tier"] == tier]
            if not rows:
                continue
            title = {"base": "基础闸门（测试与提交必须）",
                     "native": "原生库构建（ohrs → libmijia_napi.so）",
                     "hap": "HAP 打包与装机（DevEco CLT）",
                     "optional": "可选增强"}[tier]
            print("\n── %s " % title + "─" * max(0, 40 - len(title)))
            for i in rows:
                v = (" @ " + i["version"]) if i.get("version") else ""
                print("  [%s] %-22s %-28s %s" % (icon[i["status"]], i["name"], (i["detail"] or "")[:28], v))
                if i["status"] != "pass" and i.get("hint"):
                    print("      ↳ %s" % i["hint"])
        verdict = "环境满足" if n["fail"] == 0 else "环境不满足（先修 [x] 项）"
        print("\n小结：pass=%d warn=%d fail=%d → %s"
              % (n["pass"], n["warn"], n["fail"], verdict))
        print("下一步：just test（或 cargo test --manifest-path mijia-core/Cargo.toml）")
    return 0 if n["fail"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
