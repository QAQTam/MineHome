//! 会话生命周期：真探活 + passToken 静默续期的**策略**。
//!
//! 为什么要单独一层（DESIGN.md §7.6）：米家的鉴权模型是「账号即边界」，
//! server 进程持有的 `session.json` 里同时躺着 `ssecurity`（签名密钥）、
//! `serviceToken`（API 凭据）和 `passToken`（免密换回 serviceToken 的长效凭据）。
//! 「凭据过期了怎么办」因此不是一个日志问题，而是一个**权限与副作用**问题：
//!
//! - 续期本身是账号域调用（`/pass/serviceLogin` → `clientSign` → 新 serviceToken），
//!   它会**重写凭据文件**；
//! - 而过期又恰好发生在一次物理下发上时，「顺手重试」就等于**重复执行动作**
//!   （§5.1 的 at-most-once）。
//!
//! 所以这里只回答一个问题——**这次失败该不该去续期**——并且是纯函数、可穷尽单测。
//! 真正的网络动作在 `cloud.rs`，是否被允许由 env 开关决定（默认关，等用户点头）。

use crate::{CloudError, CloudResult, HomeCloud};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 疑似「凭据层面失效」（值得用 passToken 换一次新会话）的错误集合。
/// 注意 `Rejected`/`WriteRejected`（业务码）**不算**：那是设备/参数问题，
/// 拿它去触发账号域调用是无关副作用。
pub fn looks_like_expired_credentials(err: &CloudError) -> bool {
    match err {
        CloudError::NeedRelogin(_) | CloudError::CredentialInvalid => true,
        // 401 是 serviceToken 失效最常见的形状（mijia-core 把 Http 状态映射进 Transport）
        CloudError::Transport(s) => s.contains("401") || s.contains("403"),
        _ => false,
    }
}

/// 值不值得去账号域换一次新会话（不区分读写：写调用也可能**只续期不重发**）。
pub fn may_refresh(err: &CloudError, allow_refresh: bool, already_attempted: bool) -> bool {
    looks_like_expired_credentials(err) && allow_refresh && !already_attempted
}

/// 是否允许发起一次静默续期。四个条件缺一律不续（全部可单测）：
///
/// 1. 错误形状像凭据失效；
/// 2. **不是写调用**——写调用的响应丢失时无法区分「没送达」与「已执行」，
///    自动重试会重复动作，违反 §5.1；写失败只续期不重发（见 `may_reissue_after_refresh`）；
/// 3. 用户打开了续期开关（`HOME_CONTROL_ALLOW_REFRESH`，默认关）；
/// 4. 本次进程还没试过（**一次**，防成环：passToken 也死了时不能反复撞账号域）。
pub fn may_refresh_session(
    err: &CloudError,
    is_write: bool,
    allow_refresh: bool,
    already_attempted: bool,
) -> bool {
    !is_write && may_refresh(err, allow_refresh, already_attempted)
}

/// 续期成功后，原始调用可以重发的条件：只读 + 确实刷新过。
pub fn may_reissue_after_refresh(is_write: bool, refreshed: bool) -> bool {
    !is_write && refreshed
}

/// **只读调用**的「疑似失效 → 续期一次 → 重发一次」编排。
///
/// 两个原子量分开，是因为它们回答不同的问题：
/// - `attempted`：本进程**还欠不欠**一次续期（防成环：passToken 也死了时不能反复撞账号域）；
/// - `dead`：凭据是否已被**证明**不可用（续期失败，或续期成功后重发仍像过期）。
///   证明过就必须持续报 `need_relogin`——否则第二次失败只剩 `cloud_transport`，
///   把「要去人工重新登录」这个唯一有用的信息降级成网络噪声。
pub async fn read_with_refresh<T, F, Fut>(
    cloud: Arc<dyn HomeCloud>,
    allow_refresh: bool,
    attempted: &AtomicBool,
    dead: &AtomicBool,
    f: F,
) -> CloudResult<T>
where
    F: Fn(Arc<dyn HomeCloud>) -> Fut,
    Fut: Future<Output = CloudResult<T>>,
{
    // 注意：成功路径**只发一次**（这里用 match 而不是 let-else，
    // let-else 的 else 分支会跑在「成功」上，等于每次读都打两发云请求）。
    let e = match f(Arc::clone(&cloud)).await {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    if !looks_like_expired_credentials(&e) {
        return Err(e);
    }
    let owed = !attempted.load(Ordering::SeqCst) && may_refresh(&e, allow_refresh, false);
    if owed {
        attempted.store(true, Ordering::SeqCst);
        return match cloud.refresh().await {
            Ok(true) => match f(cloud).await {
                // 续期成功但重发仍像过期 ⇒ 新会话也不管用，别再试了
                Err(e2) if looks_like_expired_credentials(&e2) => {
                    dead.store(true, Ordering::SeqCst);
                    Err(need_relogin_after_dead(e2))
                }
                other => other,
            },
            Ok(false) | Err(_) => {
                dead.store(true, Ordering::SeqCst);
                Err(CloudError::NeedRelogin(
                    "passToken 静默续期失败（或被拒绝），需人工重新登录".into(),
                ))
            }
        };
    }
    if dead.load(Ordering::SeqCst) {
        return Err(need_relogin_after_dead(e));
    }
    Err(e)
}

fn need_relogin_after_dead(e: CloudError) -> CloudError {
    CloudError::NeedRelogin(format!(
        "本进程已证明凭据不可用（最后一次错误码={:?}），需人工重新登录",
        e.code()
    ))
}

/// **写调用**失败后的处理：只续期、**绝不重发**（§5.1 at-most-once）。
/// 返回是否已刷新（供调用方写进 detail，让下一次调用用上新会话）。
pub async fn refresh_without_reissue(
    cloud: &Arc<dyn HomeCloud>,
    err: &CloudError,
    allow_refresh: bool,
    attempted: &AtomicBool,
    dead: &AtomicBool,
) -> bool {
    if !may_refresh(err, allow_refresh, attempted.load(Ordering::SeqCst)) {
        return false;
    }
    attempted.store(true, Ordering::SeqCst);
    let ok = matches!(cloud.refresh().await, Ok(true));
    if !ok {
        dead.store(true, Ordering::SeqCst);
    }
    ok
}

/// 探活结果的对外措辞。**只有三种**，不写「大概还活着」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionHealth {
    /// 账号 API 实拉成功
    Alive,
    /// 凭据疑似失效：能否自愈取决于是否允许静默续期
    Expired(String),
    /// 没配 session / 网络问题 ⇒ 未知，别当成有效
    Unknown(String),
}

impl SessionHealth {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Expired(_) => "expired",
            Self::Unknown(_) => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::test_support::FakeCloud;
    use crate::{PropRef, PropSnapshot};
    use std::sync::atomic::AtomicUsize;

    /// 读一次云（`read_with_refresh` 要求的闭包形状）。
    fn read_once(
        c: Arc<dyn HomeCloud>,
    ) -> impl Future<Output = CloudResult<Vec<PropSnapshot>>> + Send + 'static {
        async move { c.get_properties("d", &[PropRef { siid: 2, piid: 1 }]).await }
    }

    fn pair() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn only_credential_shaped_failures_look_expired() {
        assert!(looks_like_expired_credentials(&CloudError::NeedRelogin("x".into())));
        assert!(looks_like_expired_credentials(&CloudError::CredentialInvalid));
        assert!(looks_like_expired_credentials(&CloudError::Transport("http 401".into())));
        assert!(looks_like_expired_credentials(&CloudError::Transport("http 403".into())));
        // 业务拒绝 / 设备侧问题：不该去动账号域
        assert!(!looks_like_expired_credentials(&CloudError::Rejected(-704040003)));
        assert!(!looks_like_expired_credentials(&CloudError::WriteRejected(400)));
        assert!(!looks_like_expired_credentials(&CloudError::VerifyMismatch));
        assert!(!looks_like_expired_credentials(&CloudError::ShapeInvalid("x".into())));
        assert!(!looks_like_expired_credentials(&CloudError::Transport("http 500".into())));
        assert!(!looks_like_expired_credentials(&CloudError::NotWired));
    }

    #[test]
    fn refresh_is_four_way_conjunction() {
        let expired = CloudError::NeedRelogin("x".into());
        assert!(may_refresh_session(&expired, false, true, false), "四条件齐 ⇒ 允许");
        assert!(
            !may_refresh_session(&expired, true, true, false),
            "写调用不许自动重发（会重复物理动作）"
        );
        assert!(
            !may_refresh_session(&expired, false, false, false),
            "开关关着不许自己打开"
        );
        assert!(
            !may_refresh_session(&expired, false, true, true),
            "每进程只试一次，passToken 也死了不能反复撞账号域"
        );
        assert!(
            !may_refresh_session(&CloudError::Rejected(9), false, true, false),
            "非凭据错误不触发"
        );
        // may_refresh 不区分读写（写路径 = 只续期不重发）
        assert!(may_refresh(&expired, true, false));
    }

    #[test]
    fn reissue_is_read_only_and_requires_a_real_refresh() {
        assert!(may_reissue_after_refresh(false, true));
        assert!(!may_reissue_after_refresh(false, false), "没续成就不重发");
        assert!(!may_reissue_after_refresh(true, true), "写调用即使续期成功也不重发");
    }

    #[tokio::test]
    async fn healthy_read_issues_exactly_one_cloud_call() {
        // 反脆弱：早前的 let-else 写法把成功路径变成第二次调用，
        // 这条就是钉死「正常时不许多发请求」。
        let fake = Arc::new(FakeCloud::new());
        let cloud: Arc<dyn HomeCloud> = fake.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let out = read_with_refresh(cloud, true, &pair().0, &pair().1, move |c| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                c.get_properties("d", &[PropRef { siid: 2, piid: 1 }]).await
            }
        })
        .await;
        assert!(out.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "成功路径必须只发一次");
    }

    #[tokio::test]
    async fn expired_read_refreshes_once_then_reissues() {
        let fake = Arc::new(FakeCloud::new());
        fake.expire_reads(1); // 第一次读 401
        let cloud: Arc<dyn HomeCloud> = fake.clone();
        let (attempted, dead) = pair();

        let out = read_with_refresh(cloud, true, &attempted, &dead, read_once).await;
        assert!(out.is_ok(), "续期一次后只读调用应重发成功：{:?}", out.err());
        assert_eq!(fake.refresh_count(), 1);
        assert!(attempted.load(Ordering::SeqCst));
        assert!(!dead.load(Ordering::SeqCst), "续期成功 ⇒ 不算已证明死掉");
    }

    #[tokio::test]
    async fn dead_passtoken_does_not_loop_but_keeps_reporting_relogin() {
        let fake = Arc::new(FakeCloud::new());
        fake.expire_reads(usize::MAX); // 永远 401
        fake.refresh_fails(); // passToken 也死了
        let (attempted, dead) = pair();

        for i in 0..5 {
            let cloud: Arc<dyn HomeCloud> = fake.clone();
            let e = read_with_refresh(cloud, true, &attempted, &dead, read_once)
                .await
                .expect_err("必须失败");
            assert_eq!(
                e.code(),
                "need_relogin",
                "第 {i} 次也要报 need_relogin，不能退化成 cloud_transport"
            );
        }
        assert_eq!(
            fake.refresh_count(),
            1,
            "每进程只撞一次账号域，不能拿过期凭据反复试"
        );
        assert!(dead.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn switch_off_means_no_account_domain_calls() {
        let fake = Arc::new(FakeCloud::new());
        fake.expire_reads(1);
        let cloud: Arc<dyn HomeCloud> = fake.clone();
        let (attempted, dead) = pair();

        let e = read_with_refresh(cloud, false, &attempted, &dead, read_once)
            .await
            .expect_err("开关关着就该把原错误抛出来");
        assert_eq!(e.code(), "cloud_transport");
        assert_eq!(fake.refresh_count(), 0, "开关关着就不该碰账号域");
        assert!(!attempted.load(Ordering::SeqCst));
        assert!(!dead.load(Ordering::SeqCst), "没试过就不许宣称已证明死掉");
    }

    #[tokio::test]
    async fn write_failure_refreshes_but_never_reissues() {
        let fake = Arc::new(FakeCloud::new());
        let cloud: Arc<dyn HomeCloud> = fake.clone();
        let (attempted, dead) = pair();
        let err = CloudError::Transport("http 401".into());

        assert!(
            refresh_without_reissue(&cloud, &err, true, &attempted, &dead).await,
            "写失败也允许把会话换回来，供下次调用使用"
        );
        assert_eq!(fake.refresh_count(), 1);
        assert!(
            !refresh_without_reissue(&cloud, &err, true, &attempted, &dead).await,
            "第二次同样的错误不再撞"
        );
        assert_eq!(fake.refresh_count(), 1, "一次进程一次，绝不循环");
        // 关键不变量：写路径的重发开关永远是关的
        assert!(!may_reissue_after_refresh(true, true));
    }

    #[tokio::test]
    async fn dead_flag_is_sticky_after_failed_refresh() {
        let fake = Arc::new(FakeCloud::new());
        fake.refresh_fails();
        let cloud: Arc<dyn HomeCloud> = fake.clone();
        let (attempted, dead) = pair();
        let err = CloudError::CredentialInvalid;

        assert!(
            !refresh_without_reissue(&cloud, &err, true, &attempted, &dead).await,
            "续期失败要如实返回 false"
        );
        assert!(dead.load(Ordering::SeqCst), "续期失败 = 已证明凭据不可用");
        assert_eq!(fake.refresh_count(), 1);
    }

    #[test]
    fn health_labels_are_exhaustive_and_honest() {
        assert_eq!(SessionHealth::Alive.label(), "alive");
        assert_eq!(SessionHealth::Expired("401".into()).label(), "expired");
        assert_eq!(
            SessionHealth::Unknown("没探过".into()).label(),
            "unknown",
            "没探过/探不了 不等于 alive"
        );
    }
}
