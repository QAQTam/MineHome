//! 门面：设计文档 §2 的 Rust API。聚合 auth / cloud / device 三层，
//! 并负责会话持久化（与 Python 原型 session.json 双向兼容）。

use crate::auth::{AuthClient, LoginOutcome};
use crate::cloud::{CloudClient, HttpSender, ReqwestSender};
use crate::device::{Device, DeviceClient, PropValue};
use crate::error::{Error, Result};
use crate::session::Session;
use crate::room::RoomClient;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use crate::auth::LoginFlow;

pub struct MiHome {
    session_path: PathBuf,
    session: Mutex<Option<Session>>,
    auth: AuthClient,
    device: DeviceClient,
    room: RoomClient,
}

impl MiHome {
    /// 生产构造：reqwest + rustls，会话文件不存在则视为未登录。
    pub fn new(session_path: impl AsRef<Path>) -> Result<Self> {
        let http: Arc<dyn HttpSender> = Arc::new(ReqwestSender::new()?);
        Ok(Self::with_http(session_path, http))
    }

    /// 注入自定义 HttpSender（测试 / 鸿蒙侧定制网络栈）
    pub fn with_http(session_path: impl AsRef<Path>, http: Arc<dyn HttpSender>) -> Self {
        let session_path = session_path.as_ref().to_path_buf();
        let session = Session::load_opt(&session_path);
        let cloud = CloudClient::new(Arc::clone(&http));
        MiHome {
            session_path,
            session: Mutex::new(session),
            auth: AuthClient::new(Arc::clone(&http), Default::default()),
            device: DeviceClient::new(cloud),
            room: RoomClient::new(CloudClient::new(Arc::clone(&http))),
        }
    }

    pub fn session(&self) -> Result<Session> {
        self.session
            .lock()
            .expect("session 锁")
            .clone()
            .ok_or(Error::NoSession)
    }

    pub fn is_logged_in(&self) -> bool {
        self.session.lock().expect("session 锁").is_some()
    }

    /// 重新从磁盘加载会话（外部导入 session.json 后调用）。
    pub fn reload_session(&self) -> Result<Session> {
        let session = Session::load(&self.session_path)?;
        *self.session.lock().expect("session 锁") = Some(session.clone());
        Ok(session)
    }

    /// 退出会话（调试用）：清空内存会话并删除磁盘会话文件。
    pub fn logout(&self) -> Result<()> {
        *self.session.lock().expect("session 锁") = None;
        match std::fs::remove_file(&self.session_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Transport(format!("删除会话文件失败: {e}"))),
        }
    }

    /// 密码登录。`LoginFlow::Direct` 时会话已持久化；
    /// `NeedNotification{url}` 时把 url 交给 ArkTS Web 组件（等手机确认），
    /// 确认完成后调 [`Self::finish_notification`]。
    pub async fn login_password(&self, user: &str, pass: &str) -> Result<LoginFlow> {
        match self.auth.login_password(user, pass).await? {
            LoginOutcome::Direct(session) => {
                self.store_session(session)?;
                Ok(LoginFlow::Direct)
            }
            LoginOutcome::NeedNotification { url } => Ok(LoginFlow::NeedNotification { url }),
        }
    }

    /// 通知确认（securityStatus=16）后的收尾：passToken → ssecurity/serviceToken。
    /// `user_id` 可传空串（此时不预设 userId cookie，以 serviceLogin 响应为准）。
    pub async fn finish_notification(
        &self,
        pass_token: &str,
        user_id: &str,
    ) -> Result<Session> {
        let session = self
            .auth
            .finish_with_pass_token(
                pass_token,
                if user_id.is_empty() { None } else { Some(user_id) },
            )
            .await?;
        self.store_session(session.clone())?;
        Ok(session)
    }

    /// 设备列表（每次实拉；缓存策略由上层决定）。
    ///
    /// 房间归属按实测结论回填：`device_list_page` **不下发 room/home 字段**，
    /// 必须另问 `/v2/homeroom/*`。归属是装饰性信息，且**用三态表达**：
    /// - `room = Some("卧室")`：云端已知，设备在该房间；
    /// - `room = Some("")`：云端归属拿到了，但该设备没进任何房间（上层显示「未分组」）；
    /// - `room = None`：`/v2/homeroom/*` 这一路失败，归属未知 —— 上层**保留上一次**
    ///   的分组，避免因一次网络失败把整页卡片重排（用户明确要求位置不许跳）。
    pub async fn list_devices(&self) -> Result<Vec<Device>> {
        let session = self.session()?;
        let mut devices = self.device.list_devices(&session).await?;
        if let Ok(rooms) = self.room.device_rooms(&session).await {
            for d in devices.iter_mut() {
                match rooms.get(&d.did) {
                    Some(r) => {
                        d.room = Some(r.room_name.clone());
                        d.room_rank = Some(r.rank);
                    }
                    None => {
                        d.room = Some(String::new());
                        d.room_rank = None;
                    }
                }
            }
        }
        Ok(devices)
    }

    /// BLE 设备 MiBeacon 广播 bindkey（32 hex）；非 BLE 设备 None
    pub async fn beaconkey(&self, did: &str) -> Result<Option<String>> {
        let session = self.session()?;
        self.device.beaconkey(&session, did).await
    }

    /// 读 spec 属性：优先米家统一通道 `/miotspec/prop/get`（全通道覆盖，
    /// 含 BLE mesh 缓存值），失败回落 rpcv2 实时穿透（WiFi 设备）。
    pub async fn get_properties(&self, did: &str, props: &[(i64, i64)]) -> Result<Vec<PropValue>> {
        let session = self.session()?;
        if let Ok(vals) = self.device.spec_get_properties(&session, did, props).await {
            if !vals.is_empty() {
                return Ok(vals);
            }
        }
        self.device.get_properties(&session, did, props).await
    }

    /// 实时读 spec 属性：**rpcv2 穿透直连设备**（控制下发后回读确认用它——
    /// `/miotspec/prop/get` 是云端缓存，mesh 上报有 1~5s 滞后，直接读会
    /// 把乐观更新的 UI 打回旧值）。`blt.*` 云拒 rpc、以及 rpcv2 失败时
    /// 回落 miotspec 缓存读。
    pub async fn get_properties_realtime(
        &self,
        did: &str,
        props: &[(i64, i64)],
    ) -> Result<Vec<PropValue>> {
        let session = self.session()?;
        if !did.starts_with("blt.") {
            if let Ok(vals) = self.device.get_properties(&session, did, props).await {
                if !vals.is_empty() {
                    return Ok(vals);
                }
            }
        }
        self.device.spec_get_properties(&session, did, props).await
    }

    pub async fn set_properties(&self, did: &str, props: &[(i64, i64, Value)]) -> Result<()> {
        let session = self.session()?;
        // 路由策略（本期实测）：
        // - `blt.*` 直连蓝牙：云 rpc 被拒（"does not support sending rpc"），直接走
        //   `/miotspec/prop/set`（等网关上报后回读生效）；
        // - 其余（WiFi/zigbee/BLE mesh 数字 did）：rpcv2 实时穿透优先；
        //   遇云侧拒绝 rpc 时降级 `/miotspec/prop/set` 重试一次。
        if did.starts_with("blt.") {
            return self.device.spec_set_properties(&session, did, props).await;
        }
        match self.device.set_properties(&session, did, props).await {
            Err(Error::Cloud { message, .. })
                if message.contains("rpc") || message.contains("bluetooth") =>
            {
                self.device.spec_set_properties(&session, did, props).await
            }
            r => r,
        }
    }

    /// 执行 spec 动作（二级卡片 action 按钮；无参动作 in=[]）
    pub async fn call_action(&self, did: &str, siid: i64, iid: i64) -> Result<Value> {
        let session = self.session()?;
        self.device.call_action(&session, did, siid, iid, &[]).await
    }

    pub async fn call_miio(&self, did: &str, method: &str, params: Value) -> Result<Value> {
        let session = self.session()?;
        self.device.call_miio(&session, did, method, params).await
    }

    fn store_session(&self, session: Session) -> Result<()> {
        session.save(&self.session_path)?;
        *self.session.lock().expect("session 锁") = Some(session);
        Ok(())
    }
}
