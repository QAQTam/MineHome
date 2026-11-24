//! # mijia-core
//!
//! 米家鸿蒙中间件核心层。分层与 API 契约见 `中间件设计.md`。
//!
//! - [`crypto`]：RC4 / nonce / sessionSecurity / 签名 / 请求加密 / 响应解密
//!   （协议细节只存在本层与 cloud 层，不向上泄漏）
//! - [`session`]：会话凭据（userId + ssecurity + serviceToken + server）与持久化
//! - [`cloud`]：RC4 签名请求器（reqwest，可注入 mock HttpSender）
//! - [`auth`]：登录（直连 / securityStatus=16 通知确认 / passToken 续期）
//! - [`device`]：设备列表缓存 / spec 属性读写 / 老 miIO rpc
//! - [`card`]：云端卡片模板引擎（解析/归一化/联动求值/spec 兜底，纯逻辑、PC 可测）
//! - [`card_net`]：getappconfig 无签名 GET + spec GET + `<filesDir>/card/` 缓存 + 编排
//! - [`room`]：家庭/房间归属（`/v2/homeroom/gethome` + `get_dev_room_page`，只读）
//! - [`safety`]：卡片安全边界（一键圆钮放行判定、控制区排除名单、文案 token 过滤）
//! - [`layout`]：首页自定义卡片（用户勾选显示哪些项 + 顺序；只过滤排序，不改形态）
//! - [`profile`]：自定义卡片的可共享档案（带 spec 语义指纹，加载时不匹配一律拒绝）
//! - [`MiHome`]：上述各层的门面（对应设计文档 §2 的 Rust API）
//!
//! 对拍基准：`fixtures/vectors.json`（由 `tools/gen_fixtures.py` 生成，
//! 金标准为实测闭环的 Python 原型 `mijia_proto.py`）。

pub mod auth;
pub mod ble_auth;
pub mod ble;
pub mod ble_cache;
pub mod card;
pub mod card_net;
pub mod cloud;
pub mod crypto;
pub mod device;
pub mod error;
pub mod layout;
pub mod mi_home;
pub mod profile;
pub mod room;
pub mod safety;
pub mod session;

pub use card::{CardEngine, CardItemVm, CardKind, CardSpec};
pub use error::{Error, Result};
pub use mi_home::{LoginFlow, MiHome};
pub use session::{Server, Session};
