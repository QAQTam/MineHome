//! 中间件 mock 测试（设计文档 §3 第二层）：
//! MockHttp 充当迷你服务端 —— 用已对拍的 [`crypto`] 在**服务端侧**重算签名、
//! 验证 rc4_hash__ 连续 keystream、解密请求体、加密响应，验证 cloud/auth/device
//! 编排逻辑与协议字段不泄漏。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use md5::{Digest, Md5};
use mijia_core::cloud::{quote_like_requests, HttpSender, HttpRequest, HttpResponse};
use mijia_core::{crypto, Error, MiHome};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

// ------------------------------------------------------------- mock 基建 ----

struct MockHttp {
    handler: Box<dyn Fn(HttpRequest) -> mijia_core::Result<HttpResponse> + Send + Sync>,
}

#[async_trait::async_trait]
impl HttpSender for MockHttp {
    async fn send(&self, req: HttpRequest) -> mijia_core::Result<HttpResponse> {
        (self.handler)(req)
    }
}

fn resp_json(status: u16, v: &Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: format!("&&&START&&&{}", serde_json::to_string(v).unwrap()),
    }
}

fn b64d(s: &str) -> Vec<u8> {
    B64.decode(s).unwrap()
}

fn form_map(req: &HttpRequest) -> HashMap<String, String> {
    req.form.as_ref().expect("signed POST 必须带 form").clone().into_iter().collect()
}

fn cookie_get<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.cookies
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// 服务端侧完整验证一个已签名 POST：URL / form 五字段 / signature / rc4_hash__
/// 连续 keystream / data 解密。返回 (app 内 path, 解密后的请求体)。
fn verify_signed_post(req: &HttpRequest, ssecurity: &str) -> (String, Value) {
    let prefix = "https://api.io.mi.com/app";
    assert!(req.url.starts_with(prefix), "url={}", req.url);
    let path = req.url[prefix.len()..].to_string();
    assert_eq!(req.method, "POST");

    let form = form_map(req);
    for k in ["data", "rc4_hash__", "signature", "_nonce", "ssecurity"] {
        assert!(form.contains_key(k), "form 缺字段 {k}");
    }
    assert_eq!(form["ssecurity"], ssecurity, "form 带明文 ssecurity");

    let sec = crypto::session_security(ssecurity, &form["_nonce"]).unwrap();

    // 密文级签名（服务端做法）
    let sig = crypto::sign(
        "POST",
        &path,
        [("data", form["data"].as_str()), ("rc4_hash__", form["rc4_hash__"].as_str())],
        &sec,
    );
    assert_eq!(sig, form["signature"], "signature 服务端验证失败: path={path}");

    // data + rc4_hash__ 必须共用同一条 keystream（先 data 后 hash）
    let mut cipher = crypto::new_request_cipher(&sec).unwrap();
    let plain = cipher.crypt(&b64d(&form["data"]));
    let plain_str = String::from_utf8(plain).unwrap();
    let hash_plain = cipher.crypt(&b64d(&form["rc4_hash__"]));
    let hash_expect = crypto::sign("POST", &path, [("data", plain_str.as_str())], &sec);
    assert_eq!(
        hash_plain,
        hash_expect.as_bytes(),
        "rc4_hash__ 明文一致性失败: path={path}"
    );

    (path, serde_json::from_str(&plain_str).unwrap())
}

/// 服务端加密响应（客户端 decrypt_response 可解）
fn enc_resp(sec_b64: &str, v: &Value) -> HttpResponse {
    let text = serde_json::to_string(v).unwrap();
    let mut c = crypto::Rc4Stream::new(&b64d(sec_b64));
    c.crypt(&[0u8; 1024]);
    HttpResponse {
        status: 200,
        headers: vec![],
        body: B64.encode(c.crypt(text.as_bytes())),
    }
}

// ------------------------------------------------------------ 登录 mock -----

const SSEC: &str = "MDEyMzQ1Njc4OWFiY2RlZg=="; // 16B 测试材料
const NONCE_DEC: &str = "1592630680355408512"; // auth2 返回的十进制 nonce
const USER_ID: &str = "900000009";
const LOCATION: &str = "https://sts.io.mi.com/sts?x=1";
const SERVICE_TOKEN: &str = "V2luZG93cy10b2tlbi10ZXN0";

/// 登录 mock：serviceLogin（按 passToken cookie 分流）/ auth2（校验 MD5 密码）/
/// clientSign GET（重算 clientSign 验证）。`step2` 决定走直连还是通知分支，
/// `token_via_cookie` 控制 serviceToken 放响应头还是 Set-Cookie（测 jar 兜底）。
fn login_mock(step2: Value, token_via_cookie: bool, expect_pass: &str) -> MockHttp {
    let expect_hash = {
        let mut h = Md5::new();
        h.update(expect_pass.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02X}")).collect::<String>()
    };
    MockHttp {
        handler: Box::new(move |req| {
            let url = req.url.clone();
            // 注意 Auth2 必须先判（是 serviceLogin 的子串）
            if url.contains("/pass/serviceLoginAuth2") {
                let form = form_map(&req);
                assert_eq!(form["user"], "user@example.com");
                assert_eq!(form["sid"], "xiaomiio");
                assert_eq!(form["_json"], "true");
                assert_eq!(form["qs"], "qs1");
                assert_eq!(form["_sign"], "sig1");
                assert_eq!(form["hash"], expect_hash, "密码必须是 MD5 大写 hex");
                return Ok(resp_json(200, &step2));
            }
            if url.contains("/pass/serviceLogin") {
                assert_eq!(req.method, "GET");
                if cookie_get(&req, "passToken").is_some() {
                    // finish_with_pass_token 分支：cookie 必须带 passToken + userId
                    assert_eq!(cookie_get(&req, "passToken"), Some("PASS_TOKEN_X"));
                    assert_eq!(cookie_get(&req, "userId"), Some(USER_ID));
                    return Ok(resp_json(
                        200,
                        &json!({"code":0, "ssecurity": SSEC, "nonce": NONCE_DEC,
                                "location": LOCATION, "userId": USER_ID}),
                    ));
                }
                return Ok(resp_json(200, &json!({"code":0, "qs":"qs1", "_sign":"sig1"})));
            }
            if url.starts_with(LOCATION) {
                // 收尾：重算 clientSign 逐字节验证
                let expect = quote_like_requests(&crypto::client_sign(NONCE_DEC, SSEC));
                assert!(
                    url.contains(&format!("clientSign={expect}")),
                    "clientSign 不匹配: {url}"
                );
                assert!(url.ends_with("&_userIdNeedEncrypt=true"), "url={url}");
                let headers = if token_via_cookie {
                    vec![(
                        "set-cookie".to_string(),
                        format!("serviceToken={SERVICE_TOKEN}; Domain=.mi.com; Path=/"),
                    )]
                } else {
                    vec![("xiaomiio_servicetoken".to_string(), SERVICE_TOKEN.to_string())]
                };
                return Ok(HttpResponse { status: 200, headers, body: "ok".into() });
            }
            Err(Error::Protocol(format!("mock 未实现的请求: {url}")))
        }),
    }
}

fn temp_session_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mijia_core_test_{}_{}.json", std::process::id(), tag))
}

// ------------------------------------------------------------------ 用例 ----

#[tokio::test]
async fn login_direct_flow() {
    let step2 = json!({"code":0, "ssecurity": SSEC, "nonce": NONCE_DEC,
                       "location": LOCATION, "userId": USER_ID});
    let path = temp_session_path("direct");
    let home = MiHome::with_http(&path, Arc::new(login_mock(step2.clone(), false, "pass1234")));

    assert!(!home.is_logged_in());
    match home.login_password("user@example.com", "pass1234").await.unwrap() {
        mijia_core::mi_home::LoginFlow::Direct => {}
        other => panic!("应为 Direct，实际 {other:?}"),
    }
    assert!(home.is_logged_in());

    let session = home.session().unwrap();
    assert_eq!(session.user_id, USER_ID);
    assert_eq!(session.ssecurity, SSEC);
    assert_eq!(session.service_token, SERVICE_TOKEN);

    // session.json 已落盘，且可被新实例加载（与 Python 原型文件互通）
    assert!(path.exists());
    let reloaded = MiHome::with_http(&path, Arc::new(login_mock(step2, false, "pass1234")));
    assert!(reloaded.is_logged_in());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn login_notification_flow() {
    // securityStatus=16：auth2 只给 notificationUrl
    let step2 = json!({"code":0, "securityStatus":16, "userId": USER_ID,
                       "notificationUrl": "https://account.xiaomi.com/notify/mobile?x=1"});
    let path = temp_session_path("notify");
    let home = MiHome::with_http(&path, Arc::new(login_mock(step2, true, "pass1234")));

    match home.login_password("user@example.com", "pass1234").await.unwrap() {
        mijia_core::mi_home::LoginFlow::NeedNotification { url } => {
            assert!(url.contains("notify"), "url={url}");
        }
        other => panic!("应为 NeedNotification，实际 {other:?}"),
    }
    assert!(!home.is_logged_in(), "通知确认前不应有会话");

    // ArkTS Web 组件完成手机确认后，拿 passToken 收尾
    let session = home
        .finish_notification("PASS_TOKEN_X", USER_ID)
        .await
        .unwrap();
    assert_eq!(session.user_id, USER_ID);
    assert_eq!(session.service_token, SERVICE_TOKEN);
    assert!(home.is_logged_in());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn login_rejected() {
    let step2 = json!({"code": 87009, "desc": "用户名或密码不正确"});
    let home = MiHome::with_http(
        temp_session_path("reject"),
        Arc::new(login_mock(step2, false, "wrong")),
    );
    match home.login_password("user@example.com", "wrong").await {
        Err(Error::Login { code, desc }) => {
            assert_eq!(code, 87009);
            assert!(desc.contains("密码"));
        }
        other => panic!("应为 Login 错误，实际 {other:?}"),
    }
}

// ------------------------------------------------------- 设备 API mock ------

fn devices_json() -> Value {
    json!({"code":0, "message":"ok", "result":{"list":[
        {"did":"900000001","model":"xiaomi.switch.w1","name":"西窗开关","isOnline":1,
         "token":"dev-token-1","spec":{"services":[{"siid":2,"piid":1}]}},
        {"did":"900000003","model":"lumi.gateway.mgl03","name":"客厅网关","isOnline":0,
         "token":"dev-token-2"},
        {"did":123456,"model":"x.spec.v3","name":"数字 did","isOnline":true},
    ]}})
}

fn device_api_mock() -> MockHttp {
    MockHttp {
        handler: Box::new(move |req| {
            let (path, body) = verify_signed_post(&req, SSEC);
            let sec = crypto::session_security(SSEC, &form_map(&req)["_nonce"]).unwrap();
            // 会话 cookie 必须带上
            assert_eq!(cookie_get(&req, "userId"), Some(USER_ID));
            assert!(cookie_get(&req, "serviceToken").is_some(), "缺 serviceToken cookie");

            match path.as_str() {
                "/v2/home/device_list_page" => {
                    assert_eq!(
                        body,
                        json!({"start_did":"0","limit":"200","get_spec":true,
                               "get_cariot_device":true,"dids":[]})
                    );
                    Ok(enc_resp(&sec, &devices_json()))
                }
                "/home/rpcv2/900000001" => match body["method"].as_str().unwrap() {
                    "set_properties" => {
                        assert_eq!(
                            body["params"][0],
                            json!({"did":"900000001","siid":2,"piid":1,"value":true})
                        );
                        Ok(enc_resp(&sec, &json!({"id":body["id"],"code":0,
                            "method":"set_properties","exe_time":12.3,
                            "ret":["ok"],"result":0})))
                    }
                    "get_properties" => {
                        assert_eq!(body["params"][0],
                            json!({"did":"900000001","siid":2,"piid":1}));
                        Ok(enc_resp(&sec, &json!({"id":body["id"],"code":0,
                            "method":"get_properties","result":[
                            {"did":"900000001","siid":2,"piid":1,"code":0,"value":true}]})))
                    }
                    m => panic!("mock 未实现的 rpcv2 method: {m}"),
                },
                // BLE 直连：云拒 rpc（本期实测口径），应触发 /miotspec/prop/set 降级
                "/home/rpcv2/blt.3.99test" => Ok(enc_resp(
                    &sec,
                    &json!({"code": -9901, "message": "bluetooth device does not support sending rpc"}),
                )),
                "/miotspec/prop/set" => {
                    assert_eq!(
                        body["params"][0],
                        json!({"did": "blt.3.99test", "siid": 2, "piid": 1, "value": true})
                    );
                    Ok(enc_resp(&sec, &json!({"code":0,"message":"ok","result":[
                        {"did":"blt.3.99test","siid":2,"piid":1,"code":1,"iid":"0.2.1"}]})))
                }
                // miotspec 缓存读：回显请求 did，value 与 rpcv2 分支一致（true）
                "/miotspec/prop/get" => {
                    let did = body["params"][0]["did"].as_str().unwrap_or("").to_string();
                    Ok(enc_resp(&sec, &json!({"code":0,"message":"ok","result":[
                        {"did":did,"siid":2,"piid":1,"code":0,"value":true,"updateTime":123}]})))
                }
                "/miotspec/action" => {
                    assert_eq!(body["params"]["did"], json!("900000002"));
                    assert_eq!(body["params"]["siid"], json!(21));
                    assert_eq!(body["params"]["aiid"], json!(1));
                    assert_eq!(body["params"]["in"], json!([]));
                    Ok(enc_resp(&sec, &json!({"code":0,"message":"ok",
                        "result":{"did":"900000002","code":0,"value":null}})))
                }
                // WiFi 设备拒绝单项（实测 eco 同值写 code=-5000）：必须上抛
                "/home/rpcv2/900000002" => {
                    assert_eq!(body["method"].as_str().unwrap(), "set_properties");
                    Ok(enc_resp(&sec, &json!({"id":body["id"],"code":0,
                        "result":[{"did":"900000002","siid":2,"piid":7,"code":-5000}]})))
                }
                "/home/rpc/900000003" => {
                    assert_eq!(body["method"], "miIO.info");
                    assert_eq!(body["params"], json!([]));
                    Ok(enc_resp(&sec, &json!({"id":body["id"],"code":0,
                        "method":"miIO.info",
                        "result":{"model":"lumi.gateway.mgl03","fw_ver":"1.4.7"}})))
                }
                _ => Ok(enc_resp(&sec, &json!({"code":401,"message":"invalid serviceToken"}))),
            }
        }),
    }
}

fn logged_home(tag: &str) -> MiHome {
    let path = temp_session_path(tag);
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "userId": USER_ID, "ssecurity": SSEC, "serviceToken": SERVICE_TOKEN
        }))
        .unwrap(),
    )
    .unwrap();
    MiHome::with_http(&path, Arc::new(device_api_mock()))
}

#[tokio::test]
async fn list_devices_round_trip() {
    let home = logged_home("devices");
    let devices = home.list_devices().await.unwrap();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0].did, "900000001");
    assert_eq!(devices[0].model, "xiaomi.switch.w1");
    assert_eq!(devices[0].name, "西窗开关");
    assert!(devices[0].online);
    assert_eq!(devices[0].token.as_deref(), Some("dev-token-1"));
    assert!(devices[0].spec.is_some());
    assert!(!devices[1].online);
    // 数字 did 兼容
    assert_eq!(devices[2].did, "123456");
    assert!(devices[2].online);
}

#[tokio::test]
async fn set_and_get_properties() {
    let home = logged_home("props");
    home
        .set_properties("900000001", &[(2, 1, json!(true))])
        .await
        .unwrap();
    let vals = home.get_properties("900000001", &[(2, 1)]).await.unwrap();
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0].siid, 2);
    assert_eq!(vals[0].piid, 1);
    assert_eq!(vals[0].value, json!(true));
}

#[tokio::test]
async fn call_miio_round_trip() {
    let home = logged_home("miio");
    let result = home.call_miio("900000003", "miIO.info", json!([])).await.unwrap();
    assert_eq!(result["model"], "lumi.gateway.mgl03");
}

#[tokio::test]
async fn cloud_error_maps_to_error_cloud() {
    let home = logged_home("err");
    // 未知路径 → mock 返回加密的 {"code":401}
    let err = home.call_miio("1", "miIO.info", json!([])).await.unwrap_err();
    match err {
        Error::Cloud { code, message } => {
            assert_eq!(code, 401);
            assert_eq!(message, "invalid serviceToken");
        }
        other => panic!("应为 Cloud 错误，实际 {other:?}"),
    }
}

#[tokio::test]
async fn set_property_bluetooth_falls_back_to_miotspec() {
    let home = logged_home("btset");
    // rpcv2 拒绝 blt.*（"does not support sending rpc"）→ 自动降级
    // /miotspec/prop/set；单项 code=1 = 已受理（异步上报），不报错
    home.set_properties("blt.3.99test", &[(2, 1, json!(true))])
        .await
        .unwrap();
}

#[tokio::test]
async fn set_property_item_code_error_surfaces() {
    let home = logged_home("itemerr");
    let err = home
        .set_properties("900000002", &[(2, 7, json!(false))])
        .await
        .unwrap_err();
    match err {
        Error::Cloud { code, message } => {
            assert_eq!(code, -5000, "设备拒绝码必须上抛: {message}");
            assert!(message.contains("5000"), "message 应带原始码: {message}");
        }
        other => panic!("应为 Cloud 错误，实际 {other:?}"),
    }
}

#[tokio::test]
async fn call_action_round_trip() {
    let home = logged_home("act");
    let v = home.call_action("900000002", 21, 1).await.unwrap();
    assert!(v.is_null(), "无 out 动作返回 null value");
}

#[tokio::test]
async fn realtime_read_prefers_rpcv2_tunnel() {
    let home = logged_home("rtl");
    // realtime：rpcv2 穿透优先（无 updateTime 字段 = 非缓存读）
    let vals = home
        .get_properties_realtime("900000001", &[(2, 1)])
        .await
        .unwrap();
    assert_eq!(vals[0].value, json!(true));
    assert_eq!(vals[0].update_time, None, "穿透读不应带缓存 updateTime");
    // blt.*：云拒 rpc，realtime 也回落 miotspec 缓存读
    let v2 = home
        .get_properties_realtime("blt.3.99test", &[(2, 1)])
        .await
        .unwrap();
    assert_eq!(v2[0].update_time, Some(123), "BLE 回落缓存读");
}
