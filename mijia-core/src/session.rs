//! 会话凭据与持久化。文件格式与 Python 原型的 `session.json` 完全兼容
//! （userId / ssecurity / serviceToken / cookies），可直接互相加载。

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 区域服务器（设计文档 §2）。当前四区 API 域名相同，账户域仅 cn 实测；
/// 其余区域留枚举位，域名差异待实测后补充。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Server {
    #[default]
    Cn,
    Sg,
    Us,
    De,
}

impl Server {
    pub fn api_base(&self) -> &'static str {
        match self {
            Server::Cn | Server::Sg | Server::Us | Server::De => "https://api.io.mi.com",
        }
    }

    pub fn account_base(&self) -> &'static str {
        match self {
            Server::Cn | Server::Sg | Server::Us | Server::De => "https://account.xiaomi.com",
        }
    }
}

/// 已完成登录的会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "ssecurity")]
    pub ssecurity: String,
    #[serde(rename = "serviceToken")]
    pub service_token: String,
    #[serde(default)]
    pub server: Server,
    /// 登录过程收到的附加 cookie（passToken 等，续期兜底用）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, String>,
}

impl Session {
    pub fn load(path: impl AsRef<Path>) -> Result<Session> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// 文件不存在时返回 None（首次使用）。
    pub fn load_opt(path: impl AsRef<Path>) -> Option<Session> {
        Self::load(path).ok()
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        Ok(std::fs::write(path, json)?)
    }

    /// 请求用 cookie 集合：userId + serviceToken 必带，附加 cookie 兜底。
    pub fn request_cookies(&self) -> Vec<(String, String)> {
        let mut out = vec![
            ("userId".to_string(), self.user_id.clone()),
            ("serviceToken".to_string(), self.service_token.clone()),
        ];
        for (k, v) in &self.cookies {
            if !matches!(k.as_str(), "userId" | "serviceToken") {
                out.push((k.clone(), v.clone()));
            }
        }
        out
    }
}
