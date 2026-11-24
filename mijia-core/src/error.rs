//! 错误类型。设计原则：协议细节错误（签名/解密）与业务错误（云 code）分层可辨，
//! 便于 napi 层映射为 ArkTS 异常。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("base64 解码失败: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP {status}: {text}")]
    Http { status: u16, text: String },

    #[error("请求发送失败: {0}")]
    Transport(String),

    #[error("协议错误: {0}")]
    Protocol(String),

    /// 登录被拒（code != 0）。code=700 需要验证码，87009 密码错误等。
    #[error("登录失败 code={code}: {desc}")]
    Login { code: i64, desc: String },

    /// 云端返回的解密后 JSON 顶层 code != 0（业务失败）。
    #[error("云响应 code={code}: {message}")]
    Cloud { code: i64, message: String },

    /// 服务端返回了无法解密/非预期格式的内容（常见于凭据失效时返回明文错误页）。
    #[error("响应不可解密 http={status} body[:200]={body}")]
    Undecryptable { status: u16, body: String },

    #[error("会话不存在（请先 login_password / finish_notification）")]
    NoSession,
}
