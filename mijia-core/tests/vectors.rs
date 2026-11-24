//! 字节级对拍：`fixtures/vectors.json` 由 `tools/gen_fixtures.py` 从金标准
//! `mijia_proto.py`（真实账号实测闭环）生成，本文件逐条断言 Rust 实现输出一致。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use mijia_core::crypto;
use serde_json::{Map, Value};

fn fixtures() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/vectors.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn section(name: &str) -> Vec<Value> {
    fixtures()[name]
        .as_array()
        .unwrap_or_else(|| panic!("夹具缺少节 {}", name))
        .clone()
}

/// JSON 对象 → (k,v) 对，按 key 逆序传入（故意打乱，验证 sign() 内部排序）。
fn pairs_reversed(obj: &Value) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = obj
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect();
    pairs.reverse();
    pairs
}

#[test]
fn rc4_vectors() {
    for c in section("rc4") {
        let out = crypto::rc4(
            &B64.decode(c["key_b64"].as_str().unwrap()).unwrap(),
            &B64.decode(c["data_b64"].as_str().unwrap()).unwrap(),
        );
        assert_eq!(
            B64.encode(&out),
            c["out_b64"].as_str().unwrap(),
            "rc4 case {}",
            c["note"].as_str().unwrap()
        );
    }
}

#[test]
fn rc4_stream_sequences() {
    for c in section("rc4_stream") {
        let mut stream =
            crypto::Rc4Stream::new(&B64.decode(c["key_b64"].as_str().unwrap()).unwrap());
        for (i, step) in c["steps"].as_array().unwrap().iter().enumerate() {
            let out = stream.crypt(&B64.decode(step["in_b64"].as_str().unwrap()).unwrap());
            if let Some(expect) = step["out_b64"].as_str() {
                assert_eq!(B64.encode(&out), expect, "stream {} step {}", c["key_b64"], i);
            }
        }
    }
}

#[test]
fn nonce_formula() {
    for c in section("nonce") {
        let mut rnd8 = [0u8; 8];
        rnd8.copy_from_slice(&B64.decode(c["rnd8_b64"].as_str().unwrap()).unwrap());
        let minute = c["minute"].as_i64().unwrap() as i32;
        assert_eq!(crypto::gen_nonce_from(rnd8, minute), c["expected"]);
    }
}

#[test]
fn session_security_vectors() {
    for c in section("session_security") {
        let got = crypto::session_security(
            c["ssecurity"].as_str().unwrap(),
            c["nonce"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(got, c["expected"].as_str().unwrap());
    }
}

#[test]
fn sign_vectors() {
    for c in section("sign") {
        let got = crypto::sign(
            c["method"].as_str().unwrap(),
            c["path"].as_str().unwrap(),
            pairs_reversed(&c["params"]),
            c["session_security"].as_str().unwrap(),
        );
        assert_eq!(
            got,
            c["expected"].as_str().unwrap(),
            "sign case {} {}",
            c["method"].as_str().unwrap(),
            c["path"].as_str().unwrap()
        );
    }
}

#[test]
fn client_sign_vectors() {
    for c in section("client_sign") {
        let got = crypto::client_sign(
            c["nonce"].as_str().unwrap(),
            c["ssecurity"].as_str().unwrap(),
        );
        assert_eq!(got, c["expected"].as_str().unwrap());
    }
}

#[test]
fn request_encrypt_vectors() {
    for c in section("request_encrypt") {
        // 1) Python 兼容 JSON 序列化（含非 ASCII \uXXXX 转义路径）
        let plain = crypto::py_json(&c["params"]);
        assert_eq!(plain, c["plain"].as_str().unwrap(), "py_json: {}", plain);

        // 2) 请求加密总装
        let got = crypto::encrypt_request(
            c["ssecurity"].as_str().unwrap(),
            c["method"].as_str().unwrap(),
            c["path"].as_str().unwrap(),
            &plain,
            c["nonce"].as_str().unwrap(),
        )
        .unwrap();

        assert_eq!(got.session_security, c["session_security"].as_str().unwrap());
        assert_eq!(got.rc4_hash_plain, c["rc4_hash"].as_str().unwrap());
        let form_map: Map<String, Value> = got
            .form
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        for (k, v) in c["form"].as_object().unwrap() {
            assert_eq!(
                form_map.get(k).and_then(|x| x.as_str()),
                Some(v.as_str().unwrap()),
                "form 字段 {} ({})",
                k,
                c["path"]
            );
        }
        // 顺序敏感：同一条 keystream 连续 crypt data → rc4_hash__
        assert_eq!(got.form[0].0, "data");
        assert_eq!(got.form[1].0, "rc4_hash__");
    }
}

#[test]
fn response_decrypt_vectors() {
    for c in section("response_decrypt") {
        let got = crypto::decrypt_response(
            c["session_security"].as_str().unwrap(),
            c["payload_b64"].as_str().unwrap(),
        )
        .unwrap();
        if let Some(expect) = c["expected_utf8"].as_str() {
            assert_eq!(String::from_utf8(got).unwrap(), expect);
        } else {
            assert_eq!(
                B64.encode(&got),
                c["expected_b64"].as_str().unwrap(),
                "binary case"
            );
        }
    }
}
