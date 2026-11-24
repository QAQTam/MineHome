//! 登录流程（sid=xiaomiio）。
//!
//! 三条路径（协议面分析报告.md §10.1，全部实测）：
//! - **A 直连**：auth2 直接返回 ssecurity/nonce/location → clientSign GET 换 serviceToken
//! - **B 通知确认**（新设备，securityStatus=16）：auth2 返回 notificationUrl，
//!   需在已登录小米账号的设备上确认。App 用 WebView 加载该 URL；确认后页面写入
//!   passToken → 本层 `finish_with_pass_token` 重放 serviceLogin 换 ssecurity/serviceToken。
//!   （ArkTS 侧用 Web 组件打开 url，等价 App 做法，比逆向确认页 H5 接口稳）
//! - **C 兜底**：auth2 返回 location 而非 notificationUrl → GET location 交换 cookie
//!   → passToken 重放 serviceLogin（原型的 B 分支）

use crate::cloud::{quote_like_requests, HttpSender, HttpRequest, UA};
use crate::crypto;
use crate::error::{Error, Result};
use crate::session::{Server, Session};
use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// 设计文档 §2 的登录结果（ArkTS 可见）
#[derive(Debug, Clone)]
pub enum LoginFlow {
    /// 已直接登录成功（session 已持久化）
    Direct,
    /// securityStatus=16：需要通知确认，ArkTS 用 Web 组件加载 url
    NeedNotification { url: String },
}

/// 内部结果（Direct 携带 session，供 MiHome 持久化）
pub enum LoginOutcome {
    Direct(Session),
    NeedNotification { url: String },
}

// ------------------------------------------------------------- CookieJar ----

/// 极简 cookie 罐：够覆盖 .xiaomi.com / io.mi.com 的登录流（Domain/Host-Only 匹配）。
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    entries: Vec<(String, bool, String, String)>, // (domain, host_only, name, value)
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(|s| s.to_string())
}

impl CookieJar {
    /// 显式写入（如 passToken，domain 形如 ".xiaomi.com" 或主机名）
    pub fn set(&mut self, domain: &str, name: &str, value: &str) {
        let host_only = !domain.starts_with('.');
        let d = domain.trim_start_matches('.').to_string();
        self.entries
            .retain(|(dd, _, n, _)| !(n == name && dd == &d));
        self.entries.push((d, host_only, name.to_string(), value.to_string()));
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(_, _, n, _)| n == name)
            .map(|(_, _, _, v)| v.as_str())
    }

    /// 从响应 Set-Cookie 收集
    pub fn observe(&mut self, url: &str, headers: &[(String, String)]) {
        let Some(host) = url_host(url) else { return };
        for (k, v) in headers {
            if !k.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let mut parts = v.split(';');
            let Some(kv) = parts.next() else { continue };
            let Some((name, value)) = kv.split_once('=') else { continue };
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            let mut domain = host.clone();
            let mut host_only = true;
            for attr in parts {
                let Some((ak, av)) = attr.split_once('=') else { continue };
                if ak.trim().eq_ignore_ascii_case("domain") {
                    domain = av.trim().trim_start_matches('.').to_string();
                    host_only = false;
                }
            }
            self.entries
                .retain(|(dd, _, n, _)| !(n == &name && dd == &domain));
            self.entries.push((domain, host_only, name, value));
        }
    }

    pub fn attach(&self, req: &mut HttpRequest) {
        let Some(host) = url_host(&req.url) else { return };
        let pairs: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|(d, h, _, _)| domain_match(d, *h, &host))
            .map(|(_, _, n, v)| (n.clone(), v.clone()))
            .collect();
        if !pairs.is_empty() {
            req.cookies = pairs;
        }
    }
}

fn domain_match(domain: &str, host_only: bool, host: &str) -> bool {
    if host_only {
        domain == host
    } else {
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

// -------------------------------------------------------------- AuthClient --

pub struct AuthClient {
    pub http: Arc<dyn HttpSender>,
    pub server: Server,
}

impl AuthClient {
    pub fn new(http: Arc<dyn HttpSender>, server: Server) -> Self {
        Self { http, server }
    }

    /// 完整密码登录。成功返回 [`LoginOutcome`]；securityStatus=16 返回
    /// `NeedNotification{url}`，等确认后调 [`Self::finish_with_pass_token`]。
    pub async fn login_password(&self, user: &str, pass: &str) -> Result<LoginOutcome> {
        let mut jar = CookieJar::default();

        // [1] serviceLogin：拿 qs/_sign
        let step1 = self
            .get_json("/pass/serviceLogin?sid=xiaomiio&_json=true", &mut jar)
            .await?;

        // [2] serviceLoginAuth2：密码 = MD5 大写 hex
        let mut hasher = Md5::new();
        hasher.update(pass.as_bytes());
        let hash = hex_upper(&hasher.finalize());
        let form = vec![
            ("user".to_string(), user.to_string()),
            ("hash".to_string(), hash),
            ("qs".to_string(), jstr(&step1, "qs")),
            ("_sign".to_string(), jstr(&step1, "_sign")),
            ("sid".to_string(), "xiaomiio".to_string()),
            ("_json".to_string(), "true".to_string()),
        ];
        let step2 = self
            .post_json("/pass/serviceLoginAuth2", form, &mut jar)
            .await?;

        let code = step2.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(Error::Login {
                code,
                desc: jstr(&step2, "desc"),
            });
        }

        // ---- A 直连 ----
        if step2.get("ssecurity").and_then(|v| v.as_str()).is_some() {
            return Ok(LoginOutcome::Direct(
                self.finish_client_sign(&step2, &mut jar).await?,
            ));
        }

        // ---- B 通知确认（securityStatus=16）----
        if let Some(url) = jstr_opt(&step2, "notificationUrl") {
            return Ok(LoginOutcome::NeedNotification { url });
        }

        // ---- C 兜底：location 交换 cookie → passToken 重放 ----
        if let Some(location) = jstr_opt(&step2, "location") {
            let req = HttpRequest {
                method: "GET".into(),
                url: location,
                headers: vec![("User-Agent".into(), UA.into())],
                form: None,
                cookies: vec![],
            };
            self.request_follow(req, &mut jar).await?;
        }
        let pass_token = jstr_opt(&step2, "passToken")
            .or_else(|| jar.get("passToken").map(|s| s.to_string()));
        if let Some(pt) = pass_token {
            let uid = jstr_opt(&step2, "userId");
            return Ok(LoginOutcome::Direct(
                self.finish_with_pass_token_inner(&pt, uid.as_deref(), &mut jar)
                    .await?,
            ));
        }

        Err(Error::Protocol(format!(
            "无法识别的登录响应（无 ssecurity/notificationUrl/location）: {}",
            mask(&step2)
        )))
    }

    /// 通知确认完成后：passToken（+可选 userId）重放 serviceLogin 换取会话。
    pub async fn finish_with_pass_token(
        &self,
        pass_token: &str,
        user_id: Option<&str>,
    ) -> Result<Session> {
        let mut jar = CookieJar::default();
        jar.set(".xiaomi.com", "passToken", pass_token);
        if let Some(uid) = user_id {
            jar.set(".xiaomi.com", "userId", uid);
        }
        self.finish_with_pass_token_inner(pass_token, user_id, &mut jar)
            .await
    }

    async fn finish_with_pass_token_inner(
        &self,
        _pass_token: &str,
        _user_id: Option<&str>,
        jar: &mut CookieJar,
    ) -> Result<Session> {
        let step = self
            .get_json("/pass/serviceLogin?sid=xiaomiio&_json=true", jar)
            .await?;
        if step.get("ssecurity").and_then(|v| v.as_str()).is_some() {
            return self.finish_client_sign(&step, jar).await;
        }
        Err(Error::Login {
            code: step.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
            desc: format!(
                "passToken 重放未换取 ssecurity: {}",
                jstr(&step, "desc")
            ),
        })
    }

    /// 登录收尾（实测钉死）：clientSign = b64(SHA1("nonce=<十进制>&"+ssecurity))，
    /// GET `location&clientSign=...&_userIdNeedEncrypt=true`，
    /// serviceToken 在响应头 `xiaomiio_serviceToken`（body 为 'ok'）。
    async fn finish_client_sign(&self, step: &Value, jar: &mut CookieJar) -> Result<Session> {
        let ssecurity = jstr_opt(step, "ssecurity")
            .ok_or_else(|| Error::Protocol("登录响应缺 ssecurity".into()))?;
        let nonce = jstr_opt(step, "nonce")
            .ok_or_else(|| Error::Protocol("登录响应缺 nonce".into()))?;
        let location = jstr_opt(step, "location")
            .ok_or_else(|| Error::Protocol("登录响应缺 location".into()))?;
        let user_id = jstr_opt(step, "userId").unwrap_or_default();

        let client_sign = crypto::client_sign(&nonce, &ssecurity);
        let url = format!(
            "{}&clientSign={}&_userIdNeedEncrypt=true",
            location,
            quote_like_requests(&client_sign)
        );
        let req = HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![("User-Agent".into(), UA.into())],
            form: None,
            cookies: vec![],
        };
        let resp = self.request_follow(req, jar).await?;

        let service_token = resp
            .header("xiaomiio_serviceToken")
            .or_else(|| resp.header("serviceToken"))
            .map(|s| s.to_string())
            .or_else(|| jar.get("serviceToken").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol(format!(
                "未拿到 serviceToken（http={}，头: {:?}）",
                resp.status,
                resp.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
            )))?;

        let mut cookies = BTreeMap::new();
        for (d, _, n, v) in &jar.entries {
            cookies
                .entry(n.clone())
                .or_insert_with(|| v.clone());
            let _ = d;
        }
        Ok(Session {
            user_id,
            ssecurity,
            service_token,
            server: self.server,
            cookies,
        })
    }

    // ------------------------------------------------------------- HTTP ----

    /// GET 账户域 JSON（自动 &&&START&&& 剥壳 + cookie 罐 + 重定向）
    async fn get_json(&self, path_query: &str, jar: &mut CookieJar) -> Result<Value> {
        let url = format!("{}{}", self.server.account_base(), path_query);
        let req = HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![("User-Agent".into(), UA.into())],
            form: None,
            cookies: vec![],
        };
        let resp = self.request_follow(req, jar).await?;
        parse_jsonp(&resp.body)
    }

    async fn post_json(
        &self,
        path: &str,
        form: Vec<(String, String)>,
        jar: &mut CookieJar,
    ) -> Result<Value> {
        let url = format!("{}{}", self.server.account_base(), path);
        let req = HttpRequest {
            method: "POST".into(),
            url,
            headers: vec![("User-Agent".into(), UA.into())],
            form: Some(form),
            cookies: vec![],
        };
        let resp = self.request_follow(req, jar).await?;
        parse_jsonp(&resp.body)
    }

    /// 跟随重定向（301/302/303 → GET；307/308 保方法），每跳收集 Set-Cookie。
    async fn request_follow(
        &self,
        mut req: HttpRequest,
        jar: &mut CookieJar,
    ) -> Result<crate::cloud::HttpResponse> {
        for _ in 0..10 {
            jar.attach(&mut req);
            let resp = self.http.send(req.clone()).await?;
            jar.observe(&req.url, &resp.headers);
            if (300..400).contains(&resp.status) {
                if let Some(loc) = resp.header("location") {
                    let base = url::Url::parse(&req.url)
                        .map_err(|e| Error::Protocol(format!("URL 解析失败: {e}")))?;
                    req.url = base
                        .join(loc)
                        .map_err(|e| Error::Protocol(format!("重定向 URL 非法: {e}")))?
                        .to_string();
                    if !matches!(resp.status, 307 | 308) {
                        req.method = "GET".into();
                        req.form = None;
                    }
                    continue;
                }
            }
            return Ok(resp);
        }
        Err(Error::Protocol("重定向次数过多（>10）".into()))
    }
}

// ---------------------------------------------------------------- helper ----

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Python `_strip_jsonp`：剥掉 &&&START&&& 前缀后解析 JSON
fn parse_jsonp(body: &str) -> Result<Value> {
    let t = body.trim();
    let t = t.strip_prefix("&&&START&&&").unwrap_or(t);
    serde_json::from_str(t)
        .map_err(|e| Error::Protocol(format!("响应非 JSON: {e}; body[:200]={}", {
            let s: String = t.chars().take(200).collect();
            s
        })))
}

fn jstr(v: &Value, key: &str) -> String {
    jstr_opt(v, key).unwrap_or_default()
}

fn jstr_opt(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// 诊断用：敏感字段掩码
fn mask(v: &Value) -> String {
    let mut out = Vec::new();
    if let Some(obj) = v.as_object() {
        for k in obj.keys() {
            let masked = matches!(
                k.as_str(),
                "ssecurity" | "passToken" | "nonce" | "serviceToken" | "psecurity" | "_sign"
            );
            out.push(if masked { format!("{k}=<masked>") } else { k.clone() });
        }
    }
    format!("keys=[{}]", out.join(","))
}
