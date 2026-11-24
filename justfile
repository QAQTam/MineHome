# MineHome 开发任务入口（just 可选；没有 just 时按每条命令注释原样手敲）
# 新机器第一步：just check —— 环境不满足，后面什么都别做。

set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

py := if os() == "windows" { "python" } else { "python3" }

default:
    @just --list --unsorted

# 环境自检（二进制/环境变量/仓库完整性，分级 pass|warn|fail）
check *args:
    {{py}} tools/check_env.py {{args}}

# 环境自检 · 机器可读版（给 CI 与 AI agent 消费）
check-json:
    {{py}} tools/check_env.py --json

# 单元 + 集成测试（不需要账号，不需要鸿蒙设备）
test:
    cargo test --manifest-path mijia-core/Cargo.toml
    cargo test --manifest-path home-control-mcp/Cargo.toml

# 泄漏扫描闸门（真实账号痕迹零容忍，豁免必须留理由）
secrets:
    {{py}} tools/check_secrets.py --all

# 提交前三条门（与 GitHub CI 完全一致）
gate: test secrets

# 重新生成协议对拍夹具（需 pip install requests pycryptodome）
vectors:
    {{py}} tools/gen_fixtures.py

# 原生库：先 export OHOS_NDK_HOME 等（见 docs/FORK-SETUP.md §2），再 ohrs build
native:
    cd mijia-napi && ohrs build

# 清 Rust 构建产物
clean:
    cargo clean --manifest-path mijia-core/Cargo.toml
    cargo clean --manifest-path home-control-mcp/Cargo.toml
