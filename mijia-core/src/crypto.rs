//! 协议加密原语（纯函数，字节级对拍 `mijia_proto.py`）。
//!
//! 来源：com.xiaomi.smarthome v11.9.705 `CloudVideoNetUtils.paramEncrypt` +
//! `_m_j/ys2|g7c|ew0|jm0`，并经真实账号实测闭环（协议面分析报告.md §10）。
//!
//! 对拍夹具：`fixtures/vectors.json`（tools/gen_fixtures.py 生成）。

use crate::error::{Error, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha1::{Digest, Sha1};
use sha2::Sha256;

// ------------------------------------------------------------------ RC4 ----

/// 标准 RC4 单发加密（对应 `_m_j/ew0` mode=3：标准 KSA + PRGA）。
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    Rc4Stream::new(key).crypt(data)
}

/// 带状态 RC4：同一条 keystream 连续 crypt 多段数据。
///
/// 米家请求加密的调用序列是：`new_request_cipher(sec)`（内部 drop 1024）→
/// `crypt(明文 data)` → `crypt(rc4_hash__明文)`，两段共用一条 keystream。
pub struct Rc4Stream {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4Stream {
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty(), "RC4 key 不能为空");
        let mut s: [u8; 256] = core::array::from_fn(|i| i as u8);
        let mut j: u8 = 0;
        for i in 0..256usize {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Self { s, i: 0, j: 0 }
    }

    pub fn crypt(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k = self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize];
            out.push(b ^ k);
        }
        out
    }
}

/// g7c 复刻：key = b64decode(sessionSecurity)，构造后先 crypt 1024 个零字节。
pub fn new_request_cipher(session_security_b64: &str) -> Result<Rc4Stream> {
    let key = B64.decode(session_security_b64)?;
    if key.len() != 32 {
        return Err(Error::Protocol(format!(
            "sessionSecurity 需解码为 32 字节，实际 {}",
            key.len()
        )));
    }
    let mut c = Rc4Stream::new(&key);
    c.crypt(&[0u8; 1024]);
    Ok(c)
}

/// micloud.decrypt_rc4 复刻：**全新** RC4(b64d(sessionSecurity)) drop 1024，
/// 解密 b64 响应体（响应也是加密的，实测确认）。
pub fn decrypt_response(session_security_b64: &str, payload_b64: &str) -> Result<Vec<u8>> {
    let key = B64.decode(session_security_b64)?;
    let mut c = Rc4Stream::new(&key);
    c.crypt(&[0u8; 1024]);
    Ok(c.crypt(&B64.decode(payload_b64)?))
}

// ---------------------------------------------------------------- nonce ----

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// gen_nonce 的确定形式（对拍用）：b64( 8B 随机 + int32_be(分钟) )。
pub fn gen_nonce_from(rnd8: [u8; 8], minute: i32) -> String {
    let mut raw = [0u8; 12];
    raw[..8].copy_from_slice(&rnd8);
    raw[8..].copy_from_slice(&minute.to_be_bytes());
    B64.encode(raw)
}

/// ys2.OooO00o 复刻：8B 安全随机 + int32((now_ms + offset)/60000)，大端，base64。
/// `server_time_offset_ms`：与云端的时钟偏差（可从 /app/v2/public/get_timestamp 校准）。
pub fn gen_nonce_with_offset(server_time_offset_ms: i64) -> String {
    let rnd: [u8; 8] = rand::random();
    // Python int() 向零截断，Rust 整数除法同语义
    let minute = ((now_ms() + server_time_offset_ms) / 60_000) as i32;
    gen_nonce_from(rnd, minute)
}

pub fn gen_nonce() -> String {
    gen_nonce_with_offset(0)
}

// ----------------------------------------------------------------- hash ----

/// sessionSecurity = b64( SHA256( b64d(ssecurity) + b64d(nonce) ) )，32B。
pub fn session_security(ssecurity_b64: &str, nonce_b64: &str) -> Result<String> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&B64.decode(ssecurity_b64)?);
    buf.extend_from_slice(&B64.decode(nonce_b64)?);
    Ok(B64.encode(Sha256::digest(&buf)))
}

/// ys2.OooO0O0 复刻：`METHOD&path&k=v(按 key 排序)&sessionSecurity` → b64(SHA1)。
///
/// - method/path 为空时跳过该段（与 Python 原型一致）；method 统一大写
/// - **注意**：`path` 不含 `/app` 前缀（如 `/v2/home/device_list_page`），实测钉死
/// - params 值必须是原始字符串（signature 对 urlencode 前的值计算）
pub fn sign<'a, I, K, V>(method: &str, path: &str, params: I, session_sec: &str) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut parts: Vec<String> = Vec::new();
    if !method.is_empty() {
        parts.push(method.to_uppercase());
    }
    if !path.is_empty() {
        parts.push(path.to_string());
    }
    let mut kvs: Vec<(String, String)> = params
        .into_iter()
        .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
        .collect();
    kvs.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in kvs {
        parts.push(format!("{}={}", k, v));
    }
    parts.push(session_sec.to_string());
    b64_sha1(parts.join("&").as_bytes())
}

/// XMPassport.getClientSign 复刻：b64(SHA1("nonce=<十进制>&" + ssecurity))。
/// `nonce_decimal` 是登录 auth2 响应里的十进制 nonce 字符串（非 b64 nonce）。
pub fn client_sign(nonce_decimal: &str, ssecurity_b64: &str) -> String {
    let raw = format!("nonce={}&{}", nonce_decimal, ssecurity_b64);
    b64_sha1(raw.as_bytes())
}

fn b64_sha1(data: &[u8]) -> String {
    B64.encode(Sha1::digest(data))
}

// ------------------------------------------------- Python 兼容 JSON 序列化 ---

/// 与 Python `json.dumps(v, separators=(",", ":"), ensure_ascii=True)` 字节一致的
/// 紧凑序列化：非 ASCII → 小写 `\uXXXX`（非 BMP → 代理对），控制字符 → 短转义。
///
/// 请求体走本函数可保证与金标准原型发出的 data 逐字节相同（签名随之一致）。
/// 注意：浮点数表示可能与 Python repr 有差异；米家请求体不含浮点，不处理。
pub fn py_json(v: &serde_json::Value) -> String {
    let mut out = String::with_capacity(64);
    write_py_value(v, &mut out);
    out
}

fn write_py_value(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_py_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_py_value(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_py_string(k, out);
                out.push(':');
                write_py_value(x, out);
            }
            out.push('}');
        }
    }
}

fn write_py_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) <= 0x7e => out.push(c),
            c => {
                let n = c as u32;
                if n < 0x10000 {
                    out.push_str(&format!("\\u{:04x}", n));
                } else {
                    let n = n - 0x10000;
                    let hi = 0xd800 | ((n >> 10) & 0x3ff);
                    let lo = 0xdc00 | (n & 0x3ff);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

// ------------------------------------------------------- 请求加密总装 -------

/// 加密签名后的请求体（form 字段按米家固定顺序）。
pub struct SignedForm {
    /// sessionSecurity（本请求的派生会话密钥，响应用它解密）
    pub session_security: String,
    pub nonce: String,
    /// rc4_hash__ 的明文（诊断用）
    pub rc4_hash_plain: String,
    /// (name, value) 有序：data, rc4_hash__, signature, _nonce, ssecurity
    pub form: Vec<(String, String)>,
}

/// `mijia_proto.api()` 加密段复刻（不含 HTTP）：
/// RC4 key 派生 → data/rc4_hash__ 连续加密 → 明文/密文两级签名 → form 五字段。
///
/// - `method`：GET/POST（会大写化）
/// - `path`：**不含** `/app` 前缀
/// - `body_json_plain`：请求体 JSON 明文（建议用 [`py_json`] 生成，保证与原型一致）
/// - `nonce`：b64 nonce（[`gen_nonce`] 生成）
pub fn encrypt_request(
    ssecurity_b64: &str,
    method: &str,
    path: &str,
    body_json_plain: &str,
    nonce: &str,
) -> Result<SignedForm> {
    let sec = session_security(ssecurity_b64, nonce)?;
    let mut cipher = new_request_cipher(&sec)?;

    let rc4_hash_plain = sign(method, path, [("data", body_json_plain)], &sec);
    let enc_data = B64.encode(cipher.crypt(body_json_plain.as_bytes()));
    let enc_hash = B64.encode(cipher.crypt(rc4_hash_plain.as_bytes()));
    let signature = sign(
        method,
        path,
        [("data", enc_data.as_str()), ("rc4_hash__", enc_hash.as_str())],
        &sec,
    );

    Ok(SignedForm {
        session_security: sec,
        nonce: nonce.to_string(),
        rc4_hash_plain,
        form: vec![
            ("data".into(), enc_data),
            ("rc4_hash__".into(), enc_hash),
            ("signature".into(), signature),
            ("_nonce".into(), nonce.to_string()),
            ("ssecurity".into(), ssecurity_b64.to_string()),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_has_fixed_shape() {
        let n = gen_nonce();
        let raw = B64.decode(&n).unwrap();
        assert_eq!(raw.len(), 12);
        let minute = i32::from_be_bytes(raw[8..].try_into().unwrap());
        let now_minute = (now_ms() / 60_000) as i32;
        assert!((now_minute - minute).abs() <= 1, "minute 应为当前时间/60000");
    }

    #[test]
    fn session_security_len_is_32() {
        let sec = session_security(
            "MDEyMzQ1Njc4OWFiY2RlZg==", // 16B
            "AQIDBAUGBwgJCgsMDQ4P",     // 12B
        )
        .unwrap();
        assert_eq!(B64.decode(&sec).unwrap().len(), 32);
    }

    #[test]
    fn rc4_invertible() {
        let key = b"some-key";
        let msg = b"hello \xE7\xB1\xB3\xE5\xAE\xB6".repeat(3); // 含 UTF-8 中文
        let enc = rc4(key, &msg);
        assert_eq!(rc4(key, &enc), msg);
    }
}
