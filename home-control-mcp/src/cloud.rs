//! 云侧接线层：`HomeCloud` ↔ `mijia_core::MiHome`（DESIGN.md §9、§7.6）。
//!
//! 已接线（只调用逐行核实过的 `MiHome` API）：
//!   - 构造 `MiHome::new(session_path)`；
//!   - `get_properties`（优先 `/miotspec/prop/get`，失败回落 rpcv2）；
//!   - `get_properties_realtime`（rpcv2 穿透，`blt.*` 自动回落缓存读）；
//!   - `set_properties`（`blt.*` 走 `/miotspec/prop/set`，其余 rpcv2 优先）；
//!   - `probe`（`list_devices` 真探活）/ `refresh`（passToken 静默续期）/
//!     `has_session_file`；
//!   - `mijia_core::error::Error → CloudError` 的**穷尽 match**（DESIGN.md §6）。
//!
//! **未接线（如实标注）**：`/device/batchdevicedatas` 的 body 形状未实测
//! （`协议面分析报告.md` §3.2 只列了路径），本期读值主路径是 `get_properties`，
//! 故 trait 里不保留这个入口——空接口比空实现更诚实。

use crate::{CloudError, CloudResult, HomeCloud, PropRef, PropSnapshot};
use mijia_core::MiHome;
use serde_json::Value;
use std::path::Path;

/// 持有 `MiHome` 门面。凭据（`Session`）只在它内部，从不出现在工具结果里。
pub struct MijiaCloud {
    home: MiHome,
}

impl MijiaCloud {
    pub fn new(session_path: impl AsRef<Path>) -> Result<Self, CloudError> {
        let home = MiHome::new(session_path).map_err(map_error)?;
        Ok(Self { home })
    }
}

/// `mijia_core::error::Error` 的穷尽映射。
/// **新增变体必须编译期报错**——ADR-0004「解析失败不得静默降级」、宪规 5。
/// 任何分支都不把 `message` / `body` / `desc` 里的原文直接回显给模型
/// （凭据失效时服务端可能返回含敏感串的明文错误页）。
fn map_error(e: mijia_core::error::Error) -> CloudError {
    use mijia_core::error::Error as E;
    match e {
        E::NoSession => CloudError::NeedRelogin("session.json 不存在或未登录".into()),
        E::Login { code, .. } => CloudError::NeedRelogin(format!("login code={code}")),
        // 响应不可解密 = 凭据失效/风控，不重试、不回显 body
        E::Undecryptable { .. } => CloudError::CredentialInvalid,
        E::Cloud { code, .. } => CloudError::Rejected(code),
        E::Protocol(s) => CloudError::ShapeInvalid(s),
        E::Json(e) => CloudError::ShapeInvalid(e.to_string()),
        E::Base64(e) => CloudError::ShapeInvalid(e.to_string()),
        E::Transport(s) => CloudError::Transport(s),
        E::Http { status, .. } => CloudError::Transport(format!("http {status}")),
        E::Io(e) => CloudError::Transport(e.to_string()),
    }
}

fn to_pairs(props: &[PropRef]) -> Vec<(i64, i64)> {
    props.iter().map(|p| (p.siid, p.piid)).collect()
}

#[async_trait::async_trait]
impl HomeCloud for MijiaCloud {
    /// 只回答「文件里有没有会话」。**不能**当健康判据（§7.6）：
    /// 过期的 `serviceToken` 在这里一样返回 true——所以本 server 启动时另外做
    /// 一次 [`Self::probe`]，并把日志字段叫 `session=` 而不是 `logged_in=`。
    async fn has_session_file(&self) -> bool {
        self.home.is_logged_in()
    }

    /// 真探活：`list_devices` 是最便宜的账号域只读调用（一次 HTTP，全设备覆盖）。
    async fn probe(&self) -> CloudResult<()> {
        self.home.list_devices().await.map(|_| ()).map_err(map_error)
    }

    /// 静默续期：`session.json` 的 `cookies.passToken` → `/pass/serviceLogin` →
    /// `clientSign` → 新 `serviceToken`（`MiHome::finish_notification` 内部已落盘）。
    ///
    /// 边界（§7.6）：
    /// - 没有 passToken ⇒ 直接 `need_relogin`，**不回退去猜密码**、不缓存猜测值；
    /// - 本方法一次调用只做一轮，不内置重试（是否调用、调几次由
    ///   `session::may_refresh_session` 决定）；
    /// - 全程不打印、不返回任何凭据片段。
    async fn refresh(&self) -> CloudResult<bool> {
        let session = self.home.session().map_err(map_error)?;
        let Some(pass_token) = session
            .cookies
            .get("passToken")
            .cloned()
            .filter(|v| !v.is_empty())
        else {
            return Err(CloudError::NeedRelogin(
                "会话里没有 passToken，无法静默续期（需人工重新登录）".into(),
            ));
        };
        match self.home.finish_notification(&pass_token, &session.user_id).await {
            Ok(_) => Ok(true),
            Err(e) => Err(map_error(e)),
        }
    }

    /// `MiHome::get_properties`：全通道缓存值 → 回落实时穿透。
    /// `PropValue.code != 0` 由调用方判失败（`read_environment` / `run_plan`），
    /// 这里只做形状投影，不吞错误、不填猜测值。
    async fn get_properties(&self, did: &str, props: &[PropRef]) -> CloudResult<Vec<PropSnapshot>> {
        let vals = self
            .home
            .get_properties(did, &to_pairs(props))
            .await
            .map_err(map_error)?;
        Ok(vals.into_iter().map(project).collect())
    }

    /// 控制下发后的回读：优先 rpcv2 实时穿透（云缓存有 1~5s 滞后）。
    async fn get_properties_realtime(
        &self,
        did: &str,
        props: &[PropRef],
    ) -> CloudResult<Vec<PropSnapshot>> {
        let vals = self
            .home
            .get_properties_realtime(did, &to_pairs(props))
            .await
            .map_err(map_error)?;
        Ok(vals.into_iter().map(project).collect())
    }

    /// `MiHome::set_properties`（内部已校验 result/ret，`device.rs`）。
    async fn set_properties(&self, did: &str, props: &[(i64, i64, Value)]) -> CloudResult<()> {
        self.home
            .set_properties(did, props)
            .await
            .map_err(|e| match e {
                // 业务码单列：写被拒与读被拒在审计里要分得开
                mijia_core::error::Error::Cloud { code, .. } => CloudError::WriteRejected(code),
                other => map_error(other),
            })
    }
}

fn project(p: mijia_core::device::PropValue) -> PropSnapshot {
    PropSnapshot {
        did: p.did,
        siid: p.siid,
        piid: p.piid,
        code: p.code,
        value: p.value,
        update_time: p.update_time,
    }
}

/// 单测用的假云（`actuate.rs` / `session.rs`）。**只在 test 构建里存在**，
/// 不会被生产二进制链接进去。
#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// `Apply` = 写入即生效（正常设备）；`Frozen` = 写了但状态不变（不生效）；
    /// `NoReads` = 读值恒空（云侧没这个属性/离线无缓存）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FakeMode {
        Apply,
        Frozen,
        NoReads,
    }

    pub struct FakeCloud {
        mode: FakeMode,
        state: Mutex<HashMap<(String, i64, i64), Value>>,
        writes: Mutex<HashMap<String, Vec<Vec<(i64, i64, Value)>>>>,
        fail_write: AtomicBool,
        has_file: AtomicBool,
        /// 前 N 次读直接 401（模拟 serviceToken 过期）
        expire_reads: AtomicUsize,
        /// passToken 也失效 ⇒ refresh 报错
        can_refresh: AtomicBool,
        refreshes: AtomicUsize,
    }

    impl FakeCloud {
        pub fn new() -> Self {
            Self::with_mode(FakeMode::Apply)
        }
        pub fn with_mode(mode: FakeMode) -> Self {
            Self {
                mode,
                state: Mutex::new(HashMap::new()),
                writes: Mutex::new(HashMap::new()),
                fail_write: AtomicBool::new(false),
                has_file: AtomicBool::new(true),
                expire_reads: AtomicUsize::new(0),
                can_refresh: AtomicBool::new(true),
                refreshes: AtomicUsize::new(0),
            }
        }
        pub fn seed(&self, did: &str, siid: i64, piid: i64, v: Value) {
            self.state
                .lock()
                .unwrap()
                .insert((did.to_string(), siid, piid), v);
        }
        pub fn fail_writes(&self) {
            self.fail_write.store(true, Ordering::SeqCst);
        }
        /// 前 `times` 次读返回 `Transport("http 401")`。
        pub fn expire_reads(&self, times: usize) {
            self.expire_reads.store(times, Ordering::SeqCst);
        }
        pub fn refresh_fails(&self) {
            self.can_refresh.store(false, Ordering::SeqCst);
        }
        pub fn without_session_file(&self) {
            self.has_file.store(false, Ordering::SeqCst);
        }
        pub fn refresh_count(&self) -> usize {
            self.refreshes.load(Ordering::SeqCst)
        }
        pub fn writes_for(&self, did: &str) -> Vec<Vec<(i64, i64, Value)>> {
            self.writes
                .lock()
                .unwrap()
                .get(did)
                .cloned()
                .unwrap_or_default()
        }
        fn read(&self, did: &str, props: &[PropRef]) -> CloudResult<Vec<PropSnapshot>> {
            let left = self.expire_reads.load(Ordering::SeqCst);
            if left > 0 {
                self.expire_reads.store(left - 1, Ordering::SeqCst);
                return Err(CloudError::Transport("http 401".into()));
            }
            if self.mode == FakeMode::NoReads {
                return Ok(Vec::new());
            }
            let st = self.state.lock().unwrap();
            Ok(props
                .iter()
                .filter_map(|p| {
                    st.get(&(did.to_string(), p.siid, p.piid))
                        .map(|v| PropSnapshot {
                            did: did.to_string(),
                            siid: p.siid,
                            piid: p.piid,
                            code: 0,
                            value: v.clone(),
                            update_time: None,
                        })
                })
                .collect())
        }
    }

    impl Default for FakeCloud {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait::async_trait]
    impl HomeCloud for FakeCloud {
        async fn has_session_file(&self) -> bool {
            self.has_file.load(Ordering::SeqCst)
        }
        async fn probe(&self) -> CloudResult<()> {
            if self.expire_reads.load(Ordering::SeqCst) > 0 {
                return Err(CloudError::Transport("http 401".into()));
            }
            if self.has_file.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(CloudError::NeedRelogin("fake: 没有会话文件".into()))
            }
        }
        async fn refresh(&self) -> CloudResult<bool> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            if self.can_refresh.load(Ordering::SeqCst) {
                self.expire_reads.store(0, Ordering::SeqCst);
                self.has_file.store(true, Ordering::SeqCst);
                Ok(true)
            } else {
                Err(CloudError::NeedRelogin("fake: passToken 也失效".into()))
            }
        }
        async fn get_properties(&self, did: &str, props: &[PropRef]) -> CloudResult<Vec<PropSnapshot>> {
            self.read(did, props)
        }
        async fn get_properties_realtime(
            &self,
            did: &str,
            props: &[PropRef],
        ) -> CloudResult<Vec<PropSnapshot>> {
            self.read(did, props)
        }
        async fn set_properties(&self, did: &str, props: &[(i64, i64, Value)]) -> CloudResult<()> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(CloudError::WriteRejected(-1));
            }
            self.writes
                .lock()
                .unwrap()
                .entry(did.to_string())
                .or_default()
                .push(props.to_vec());
            if self.mode == FakeMode::Apply {
                let mut st = self.state.lock().unwrap();
                for (s, p, v) in props {
                    st.insert((did.to_string(), *s, *p), v.clone());
                }
            }
            Ok(())
        }
    }
}
