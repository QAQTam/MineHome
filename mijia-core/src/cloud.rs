//! RC4 签名请求器。协议细节（rc4_hash__ 等字段）封闭在本模块，向上只暴露
//! `post_signed(session, path, body) -> Value`。
//!
//! HTTP 通过 [`HttpSender`] trait 注入：生产用 [`ReqwestSender`]（不跟随重定向，
//! 由调用方控制以便观察每跳 Set-Cookie），测试注入 mock 做服务端校验。

use crate::crypto;
use crate::error::{Error, Result};
use crate::session::Session;
use async_trait::async_trait;
use serde_json::Value;

/// 与 App/Python 原型一致的 UA（实测必需）
pub const UA: &str = "Android/11.9.705 (xiaomi.io; Android 15; 110917051) rf/passtoken/1.0.1";

/// 与 requests.data 等价的 form 编码（application/x-www-form-urlencoded，空格→+）
pub fn encode_form(form: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish()
}

/// Python requests.utils.quote（safe='/'）等价的查询值编码：clientSign 用
pub fn quote_like_requests(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ------------------------------------------------------------- HTTP 抽象 ----

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// application/x-www-form-urlencoded 请求体
    pub form: Option<Vec<(String, String)>>,
    /// Cookie 头内容（name=value 列表）
    pub cookies: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// 头名小写
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[async_trait]
pub trait HttpSender: Send + Sync {
    /// 单次 HTTP 交换（**不**跟随重定向；重定向由 auth 层控制）
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse>;
}

/// 生产实现：reqwest + rustls（ohos 交叉编译友好）
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl ReqwestSender {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl HttpSender for ReqwestSender {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| Error::Protocol(format!("非法 method: {e}")))?;
        let mut rb = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(form) = &req.form {
            rb = rb.header("Content-Type", "application/x-www-form-urlencoded");
            rb = rb.body(encode_form(form));
        }
        if !req.cookies.is_empty() {
            let head = req
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            rb = rb.header("Cookie", head);
        }
        let resp = rb.send().await.map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = resp.text().await.map_err(|e| Error::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

// ------------------------------------------------------------- 请求器 ------

pub struct CloudClient {
    pub http: std::sync::Arc<dyn HttpSender>,
}

impl CloudClient {
    pub fn new(http: std::sync::Arc<dyn HttpSender>) -> Self {
        Self { http }
    }

    /// 已签名 POST：`https://api.io.mi.com/app<path>`，RC4 加密 form 五字段，
    /// 响应解密为 JSON（响应也是加密的，实测确认）。
    pub async fn post_signed(&self, session: &Session, path: &str, body: &Value) -> Result<Value> {
        let nonce = crypto::gen_nonce();
        let plain = crypto::py_json(body);
        let signed = crypto::encrypt_request(&session.ssecurity, "POST", path, &plain, &nonce)?;
        let url = format!("{}/app{}", session.server.api_base(), path);

        let mut headers = vec![
            ("User-Agent".to_string(), UA.to_string()),
            // 实测非必需，加上与 App 对齐更稳
            ("x-xiaomi-protocal-flag".to_string(), "2".to_string()),
        ];
        let resp = self
            .http
            .send(HttpRequest {
                method: "POST".into(),
                url,
                headers: std::mem::take(&mut headers),
                form: Some(signed.form.clone()),
                cookies: session.request_cookies(),
            })
            .await?;

        if let Ok(dec) = crypto::decrypt_response(&signed.session_security, resp.body.trim()) {
            return Ok(serde_json::from_slice(&dec)?);
        }
        // 凭据失效/风控时服务端可能返回明文错误 JSON
        if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
            if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
                if code != 0 {
                    return Err(Error::Cloud {
                        code,
                        message: v
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
            }
        }
        Err(Error::Undecryptable {
            status: resp.status,
            body: resp.body.chars().take(200).collect(),
        })
    }
}
