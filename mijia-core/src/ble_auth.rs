//! 米家标准蓝牙认证（mible）+ SPEC v2 数据通道（0x001B 加解密与帧解析）。
//!
//! 全部由米家 APK 反编译钉死并在酷态科 10号 Ultra（`njcuk.fitting.ad1204`）真机
//! 闭环验证（MIC 60/60），协议细节见 `docs/analysis/蓝牙标准认证协议.md` §8：
//!
//! - 登录密钥：`okm = HKDF-SHA256(ikm=token12B, salt=app_rand‖dev_rand,
//!   info=b"mible-login-info", L=64)`（`standardauth/OooO0OO.java`）
//! - confirm：`dev = HMAC(okm[0:16], dev‖app)`、`app = HMAC(okm[16:32], app‖dev)`
//! - 0x001B 帧：`[sn u16 LE][密文 (N-4)B][MIC 4B]`，AES-CCM（RFC3610，M=4、
//!   L=3、无 AAD；BouncyCastle CCMBlockCipher，`i3g.OooOo0()`）
//!   - RX：key=`okm[0:16]`，nonce(12B)=`okm[32:36]‖0000‖sn_LE‖roll_LE`
//!   - TX：key=`okm[16:32]`，nonce(12B)=`okm[36:40]‖0000‖sn_LE‖roll_LE`
//!   - `sn` bit15 是 roll 翻转位（`d71.java`）：相邻两帧 bit15 翻转一次 roll+1，
//!     RX/TX 各自独立计数，连接建立时清零
//! - 明文 = miot SPEC v2 帧（`egh.Oooo0`/`cic`/`eyb`）：
//!   `(ver<<12)|len`(u16LE)、`tid`(u16LE)、`opcode`(u8)、objects…
//!   object = `[siid u8][piid u16LE][vtype<<12|vlen u16LE][value LE]`
//! - 端口信息 u32（实测钉死）：LE 字节 `[state][协商上限][I×0.1A][V×0.1V]`

use sha2::{Digest, Sha256};

// ------------------------------------------------------------- HKDF/HMAC ----

/// HMAC-SHA256（RFC2104，自实现避免引入 hmac crate）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([&ipad, msg].concat());
    let mut buf = opad;
    buf.extend_from_slice(&inner);
    Sha256::digest(&buf).into()
}

/// HKDF-SHA256（RFC5869 extract+expand）。
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hmac_sha256(salt, ikm);
    let mut t: Vec<u8> = Vec::new();
    let mut okm = Vec::with_capacity(len);
    let mut i = 1u8;
    while okm.len() < len {
        let mut buf = t.clone();
        buf.extend_from_slice(info);
        buf.push(i);
        t = hmac_sha256(&prk, &buf).to_vec();
        okm.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    okm.truncate(len);
    okm
}

// --------------------------------------------------------------- session ----

/// 登录后的会话密钥（64B okm，四段用途见模块注释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MibleSession {
    pub okm: [u8; 64],
}

impl MibleSession {
    /// `token`：设备 12B 云 token；`app_rand`/`dev_rand`：登录双向随机数（各 16B）。
    pub fn derive(token: &[u8], app_rand: &[u8], dev_rand: &[u8]) -> Self {
        let mut salt = Vec::with_capacity(app_rand.len() + dev_rand.len());
        salt.extend_from_slice(app_rand);
        salt.extend_from_slice(dev_rand);
        let okm = hkdf_sha256(token, &salt, b"mible-login-info", 64);
        Self {
            okm: okm.try_into().expect("hkdf len=64"),
        }
    }

    /// devConfirm（app 校验设备）：HMAC(okm[0:16], dev‖app)。
    pub fn dev_confirm(&self, app_rand: &[u8], dev_rand: &[u8]) -> [u8; 32] {
        let mut msg = dev_rand.to_vec();
        msg.extend_from_slice(app_rand);
        hmac_sha256(&self.okm[0..16], &msg)
    }

    /// appConfirm（设备校验 app）：HMAC(okm[16:32], app‖dev)。
    pub fn app_confirm(&self, app_rand: &[u8], dev_rand: &[u8]) -> [u8; 32] {
        let mut msg = app_rand.to_vec();
        msg.extend_from_slice(dev_rand);
        hmac_sha256(&self.okm[16..32], &msg)
    }
}

// -------------------------------------------------------------- AES-CCM ----

const MIC_LEN: usize = 4;

/// RFC3610 AES-CCM 解密+校验（M=4，L=15-nonce.len()，无 AAD）。MIC 不符返回 None。
fn ccm_decrypt(key: &[u8; 16], nonce: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < MIC_LEN {
        return None;
    }
    let (ct, mic) = data.split_at(data.len() - MIC_LEN);
    let pt = ccm_keystream(key, nonce, ct);
    let tag = ccm_tag(key, nonce, &pt);
    if tag == mic {
        Some(pt)
    } else {
        None
    }
}

/// RFC3610 AES-CCM 加密（无 AAD），返回 密文‖MIC4。
fn ccm_encrypt(key: &[u8; 16], nonce: &[u8], pt: &[u8]) -> Vec<u8> {
    let mut out = ccm_keystream(key, nonce, pt);
    out.extend_from_slice(&ccm_tag(key, nonce, pt));
    out
}

/// CTR 段（A_1.. 起，A_0 仅产 S0 掩码）。
fn ccm_keystream(key: &[u8; 16], nonce: &[u8], ct: &[u8]) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let ecb = aes::Aes128::new_from_slice(key).expect("16B key");
    let q = 15 - nonce.len();
    let mut out = ct.to_vec();
    let mut ctr: u64 = 1;
    for chunk in out.chunks_mut(16) {
        let mut block = [0u8; 16];
        block[0] = (q - 1) as u8; // A_i Flags：bit7=0，低 3 位 = q-1
        block[1..1 + nonce.len()].copy_from_slice(nonce);
        let cbe = ctr.to_be_bytes();
        block[16 - q..].copy_from_slice(&cbe[8 - q..]);
        ecb.encrypt_block((&mut block).into());
        for (o, k) in chunk.iter_mut().zip(block.iter()) {
            *o ^= k;
        }
        ctr += 1;
    }
    out
}

/// CBC-MAC（B0 + 明文块，无 AAD）⊕ S0 —— 标准 CCM MIC（区别于 ble.rs 里 MiBeacon
/// 的 ccm_star_tag：那里 AAD=0x11 且 B0 带 Adata 位，是 MiBeacon 专用变体）。
fn ccm_tag(key: &[u8; 16], nonce: &[u8], pt: &[u8]) -> [u8; MIC_LEN] {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let ecb = aes::Aes128::new_from_slice(key).expect("16B key");
    let q = 15 - nonce.len();
    let m2 = ((MIC_LEN - 2) / 2) as u8;
    let mut b0 = [0u8; 16];
    b0[0] = (m2 << 3) | ((q - 1) as u8); // 无 AAD → bit6=0
    b0[1..1 + nonce.len()].copy_from_slice(nonce);
    let lbe = (pt.len() as u64).to_be_bytes();
    b0[16 - q..].copy_from_slice(&lbe[8 - q..]);
    let mut x = b0;
    ecb.encrypt_block((&mut x).into());
    for chunk in pt.chunks(16) {
        for (xi, bi) in x.iter_mut().zip(chunk.iter()) {
            *xi ^= bi;
        }
        ecb.encrypt_block((&mut x).into());
    }
    let s0 = {
        let mut a0 = [0u8; 16];
        a0[0] = (q - 1) as u8;
        a0[1..1 + nonce.len()].copy_from_slice(nonce);
        let mut a0 = a0;
        ecb.encrypt_block((&mut a0).into());
        a0
    };
    let mut tag = [0u8; MIC_LEN];
    for (t, (a, b)) in tag.iter_mut().zip(x.iter().zip(s0.iter())) {
        *t = a ^ b;
    }
    tag
}

// --------------------------------------------------- 0x001B 通道加解密 ----

/// spec 通道收包状态（镜像 APK `_m_j/d71.java`：rx_sn / rx_roll）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpecRxState {
    pub last_sn: u16,
    pub roll: u16,
}

impl SpecRxState {
    /// 解一帧 0x001B 推送：`frame = [sn u16 LE][密文‖MIC]`。MIC 不符返回 None。
    pub fn decrypt(&mut self, frame: &[u8], session: &MibleSession) -> Option<Vec<u8>> {
        if frame.len() < 2 + MIC_LEN {
            return None;
        }
        let sn = u16::from_le_bytes([frame[0], frame[1]]);
        if ((self.last_sn & 0x8000) >> 15) != ((sn & 0x8000) >> 15) {
            self.roll = self.roll.wrapping_add(1);
        }
        self.last_sn = sn;
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&session.okm[32..36]);
        nonce[8..10].copy_from_slice(&sn.to_le_bytes());
        nonce[10..12].copy_from_slice(&self.roll.to_le_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&session.okm[0..16]);
        ccm_decrypt(&key, &nonce, &frame[2..])
    }
}

/// spec 通道发包状态（tx_sn / tx_roll，与 RX 相互独立）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpecTxState {
    pub sn: u16,
    pub roll: u16,
}

impl SpecTxState {
    /// 组一帧 0x001B 下行：`[sn u16 LE][密文‖MIC]`（key=okm[16:32]，nonce 前缀 okm[36:40]）。
    pub fn encrypt(&mut self, plaintext: &[u8], session: &MibleSession) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&session.okm[36..40]);
        nonce[8..10].copy_from_slice(&self.sn.to_le_bytes());
        nonce[10..12].copy_from_slice(&self.roll.to_le_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&session.okm[16..32]);
        let mut out = self.sn.to_le_bytes().to_vec();
        out.extend_from_slice(&ccm_encrypt(&key, &nonce, plaintext));
        let next = self.sn.wrapping_add(1);
        if ((self.sn & 0x8000) >> 15) != ((next & 0x8000) >> 15) {
            self.roll = self.roll.wrapping_add(1);
        }
        self.sn = next;
        out
    }
}

// --------------------------------------------------- TX 组帧 / 快路径 ----

/// mible 0x0010 登录命令帧 `[0x24 00 00 00]`。
pub const LOGIN_CMD: [u8; 4] = [0x24, 0x00, 0x00, 0x00];

/// spec v2 get 请求明文帧（APK `_m_j/kt4.java` 组帧器逆向，2026-11-23 实机写入被
/// 物理层接受）：`(2<<12)|len`(u16 LE)、`tid`(u16 LE)、`0x02`、`count` +
/// count×`[siid u8][piid u16 LE]`。len = 整帧长度 = 6 + 3*count（ver 帧 0x2005
/// 同口径：5 = 头2+tid2+op1）。
/// 明文帧再交 [`SpecTxState::encrypt`] 加密成 0x001A 下行。
pub fn build_spec_get(oids: &[(u8, u16)], tid: u16) -> Vec<u8> {
    let total = 6 + 3 * oids.len();
    let mut out = Vec::with_capacity(7 + 3 * oids.len());
    out.extend_from_slice(&(((2u16 << 12) | total as u16).to_le_bytes()));
    out.extend_from_slice(&tid.to_le_bytes());
    out.push(0x02);
    out.push(oids.len() as u8);
    for (siid, piid) in oids {
        out.push(*siid);
        out.extend_from_slice(&piid.to_le_bytes());
    }
    out
}

/// spec v2 版本交换明文帧：`(2<<12)|5`、`tid`、`0xF0`（APK `r5e.java`，R2 常量
/// 8197=0x2005 交叉验证）。连接建立后先发一帧换版本再走数据面。
pub fn build_spec_ver(tid: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(&(((2u16 << 12) | 5).to_le_bytes()));
    out.extend_from_slice(&tid.to_le_bytes());
    out.push(0xF0);
    out
}

/// 0x0019 分片流上行帧序列：CMD start `[0000 00 type n]` + 数据帧
/// `[seq u16 LE][≤18B chunk]`。`type`：11=appRandom、10=appConfirm、9=bindkey。
pub fn build_stream_frames(pkg_type: u8, payload: &[u8]) -> Vec<Vec<u8>> {
    let n = payload.len().div_ceil(18).max(1);
    let mut out = Vec::with_capacity(1 + n);
    let mut start = vec![0x00u8, 0x00, 0x00, pkg_type];
    start.extend_from_slice(&(n as u16).to_le_bytes());
    out.push(start);
    for (i, chunk) in payload.chunks(18).enumerate() {
        let mut f = ((i as u16) + 1).to_le_bytes().to_vec();
        f.extend_from_slice(chunk);
        out.push(f);
    }
    out
}

/// 0x0019 流 ACK 帧 `[0000 01 status]`（1=收到 CMD start，0=数据帧收齐）。
pub fn build_stream_ack(status: u8) -> Vec<u8> {
    vec![0x00, 0x00, 0x01, status]
}

/// ebe0ccc1 免登录读数（秒秒测家族 Current Temp and Humi）。2026-11-23 06 实机
/// 定案 5 字节：`[temp int16 LE ÷100][humi u8][电压 int16 LE ÷1000]`，
/// 与 MJWSD05MMC dump 交叉验证自洽（见固件分析 §4.4）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HtReading {
    pub temp_c: f32,
    pub humidity: f32,
    pub voltage_v: f32,
}

pub fn parse_ebe0ccc1(data: &[u8]) -> Option<HtReading> {
    if data.len() < 5 {
        return None;
    }
    let t = i16::from_le_bytes([data[0], data[1]]);
    Some(HtReading {
        temp_c: t as f32 / 100.0,
        humidity: data[2] as f32,
        voltage_v: i16::from_le_bytes([data[3], data[4]]) as f32 / 1000.0,
    })
}

/// ebe0ccc4 电量（u8，%）。
pub fn parse_ebe0ccc4(data: &[u8]) -> Option<u8> {
    data.first().copied()
}

// ------------------------------------------------------- SPEC v2 帧解析 ----

/// spec v2 opcode（miot-sdk blespec.js）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecOpcode {
    SetProperties,
    SetPropertiesRsp,
    GetProperties,
    GetPropertiesRsp,
    PropertiesChanged,
    Action,
    ActionRsp,
    EventOccured,
    VersionExchange,
    Unknown(u8),
}

impl SpecOpcode {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::SetProperties,
            1 => Self::SetPropertiesRsp,
            2 => Self::GetProperties,
            3 => Self::GetPropertiesRsp,
            4 => Self::PropertiesChanged,
            5 => Self::Action,
            6 => Self::ActionRsp,
            7 => Self::EventOccured,
            0xF0 => Self::VersionExchange,
            other => Self::Unknown(other),
        }
    }
}

/// SpecValueType（com/miot/spec/entity/SpecValueType.java）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecValueType {
    Bool,
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    Float,
    String,
}

impl SpecValueType {
    fn from_nibble(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Bool,
            1 => Self::Uint8,
            2 => Self::Int8,
            3 => Self::Uint16,
            4 => Self::Int16,
            5 => Self::Uint32,
            6 => Self::Int32,
            7 => Self::Uint64,
            8 => Self::Int64,
            9 => Self::Float,
            10 => Self::String,
            _ => return None,
        })
    }

    fn byte_len(&self) -> usize {
        match self {
            Self::Bool | Self::Uint8 | Self::Int8 => 1,
            Self::Uint16 | Self::Int16 => 2,
            Self::Uint32 | Self::Int32 | Self::Float => 4,
            Self::Uint64 | Self::Int64 => 8,
            Self::String => 0, // 变长，帧内 vlen 决定
        }
    }
}

/// 解析出的属性对象（value 为帧内原始小端字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecObject {
    pub siid: u8,
    pub piid: u16,
    pub value_type: SpecValueType,
    pub value: Vec<u8>,
}

impl SpecObject {
    /// value 按 SpecValueType 小端解释为 u64（bool → 0/1）。
    pub fn value_u64(&self) -> Option<u64> {
        if self.value.len() != self.value_type.byte_len() {
            return None;
        }
        Some(match self.value_type {
            SpecValueType::Bool => (self.value[0] == 1) as u64,
            _ => {
                let mut le = self.value.clone();
                le.reverse(); // 小端 → 大端
                let mut v = 0u64;
                for b in le {
                    v = (v << 8) | b as u64;
                }
                v
            }
        })
    }
}

/// 解析出的 spec v2 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecFrame {
    /// 协议版本（(ver<<12)|len 的高 4 位，>2 抛 ProtocolVersionIsLow）。
    pub version: u8,
    pub tid: u16,
    pub opcode: SpecOpcode,
    pub objects: Vec<SpecObject>,
}

/// 解析 spec v2 明文帧（`egh.Oooo0` + `eyb.OooO0OO` 等价实现）。
/// opcode=4（properties_changed）/7（event_occured）解析 objects；0xF0 为版本交换帧。
pub fn parse_spec_frame(pt: &[u8]) -> Option<SpecFrame> {
    if pt.len() < 5 {
        return None;
    }
    let hdr = u16::from_le_bytes([pt[0], pt[1]]);
    let version = (hdr >> 12) as u8;
    let _frame_len = (hdr & 0x0FFF) as usize;
    let tid = u16::from_le_bytes([pt[2], pt[3]]);
    let opcode = SpecOpcode::from_u8(pt[4]);
    if version > 2 {
        return None; // ProtocolVersionIsLowException
    }
    let mut objects = Vec::new();
    if matches!(opcode, SpecOpcode::PropertiesChanged | SpecOpcode::EventOccured) {
        let buf = &pt[5..];
        if buf.is_empty() {
            return None;
        }
        let count = buf[0] as usize;
        let mut pos = 1;
        for _ in 0..count {
            // object 头最少 5B：siid1 + piid2 + (type|len)2
            if pos + 5 > buf.len() {
                return None;
            }
            let siid = buf[pos];
            let piid = u16::from_le_bytes([buf[pos + 1], buf[pos + 2]]);
            let tl = u16::from_le_bytes([buf[pos + 3], buf[pos + 4]]);
            let vtype = SpecValueType::from_nibble((tl >> 12) as u8)?;
            let vlen = (tl & 0x0FFF) as usize;
            pos += 5;
            if pos + vlen > buf.len() {
                return None;
            }
            objects.push(SpecObject {
                siid,
                piid,
                value_type: vtype,
                value: buf[pos..pos + vlen].to_vec(),
            });
            pos += vlen;
        }
    }
    Some(SpecFrame {
        version,
        tid,
        opcode,
        objects,
    })
}

// ------------------------------------------------------------ 端口信息 ----

/// port-x-info uint32 位域（真机差分钉死，见协议文档 §8.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortInfo {
    /// 端口 state（实测输出中恒 1；空载端口不推送）。
    pub state: u8,
    /// 协商上限字段（实测不随负载变化；PD 口上=电流上限×0.1A 或合同功率 W，
    /// 非 PD 口（QC 族）单位未定——读 V/I 不需要它）。
    pub contract: u8,
    /// 实测电流，0.1A 单位（如 11 = 1.1A）。
    pub current_x10a: u8,
    /// 实测电压，0.1V 单位（如 91 = 9.1V）。
    pub voltage_x10v: u8,
}

impl PortInfo {
    /// 从 siid=2 的 port-x-info u32 解出（LE 字节 [state][contract][I][V]）。
    pub fn from_u32(v: u32) -> Self {
        Self {
            state: (v & 0xFF) as u8,
            contract: ((v >> 8) & 0xFF) as u8,
            current_x10a: ((v >> 16) & 0xFF) as u8,
            voltage_x10v: ((v >> 24) & 0xFF) as u8,
        }
    }

    /// 电压（V，如 9.1）。
    pub fn voltage_v(&self) -> f32 {
        self.voltage_x10v as f32 / 10.0
    }

    /// 电流（A，如 1.1）。
    pub fn current_a(&self) -> f32 {
        self.current_x10a as f32 / 10.0
    }

    /// 近似功率（V×I，W）。
    pub fn power_w(&self) -> f32 {
        self.voltage_v() * self.current_a()
    }
}

// ---------------------------------------------------------------- tests ----

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成 golden 向量（Python 侧 pycryptodome 生成；token/随机数均为假数据，
    /// 不含真实凭据）。覆盖 HKDF/confirm/CCM/spec 解析/端口解码全链。
    const GOLDEN: &str = r#"{
      "note": "synthetic golden vector (no real credentials)",
      "token": "aabbccddeeff0011223344aa",
      "app_rand": "00112233445566778899aabbccddeeff",
      "dev_rand": "ffeeddccbbaa99887766554433221100",
      "okm": "7ff8b525b1674a3b1f3be4e1c34a77f2e6a1472cc289e6948645fe1c42e6b71688bd87842c62f0d420945ab94bcdeb50265d59aabf7dd19d19f6d6cccab2e608",
      "dev_confirm": "71e2fe18c2ede51dc172822a4b9c9e191e160017f3235b63e81b75a8d1185396",
      "app_confirm": "e0cc4112044a5958670e9725d17a392caee1acd7c4e1db9d67e0f451d703e365",
      "frames": [
        "01003179a204316b817d11a0eb1fdb2ee41559ed15",
        "020000576138cfc74ff43672a67b4713a2a0be0bca",
        "03002602a2464d1c8b4d567be8973e10fdb76ae26f"
      ],
      "expect": [
        {"sn": 1, "tid": 1, "opcode": 4, "siid": 2, "piid": 1, "u32": "5b090a01"},
        {"sn": 2, "tid": 2, "opcode": 4, "siid": 2, "piid": 1, "u32": "5b0b0a01"},
        {"sn": 3, "tid": 3, "opcode": 4, "siid": 2, "piid": 1, "u32": "5a090a01"}
      ]
    }"#;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn golden_hkdf_and_confirms() {
        let v: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        let token = unhex(v["token"].as_str().unwrap());
        let app = unhex(v["app_rand"].as_str().unwrap());
        let dev = unhex(v["dev_rand"].as_str().unwrap());
        let session = MibleSession::derive(&token, &app, &dev);
        assert_eq!(session.okm.to_vec(), unhex(v["okm"].as_str().unwrap()));
        assert_eq!(
            session.dev_confirm(&app, &dev).to_vec(),
            unhex(v["dev_confirm"].as_str().unwrap())
        );
        assert_eq!(
            session.app_confirm(&app, &dev).to_vec(),
            unhex(v["app_confirm"].as_str().unwrap())
        );
    }

    #[test]
    fn golden_rx_frames() {
        let v: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        let app = unhex(v["app_rand"].as_str().unwrap());
        let dev = unhex(v["dev_rand"].as_str().unwrap());
        let session = MibleSession::derive(unhex(v["token"].as_str().unwrap()).as_slice(), &app, &dev);
        let mut rx = SpecRxState::default();
        for (frame_hex, expect) in v["frames"].as_array().unwrap().iter().zip(
            v["expect"].as_array().unwrap().iter(),
        ) {
            let frame = unhex(frame_hex.as_str().unwrap());
            let pt = rx.decrypt(&frame, &session).expect("MIC verify");
            assert_eq!(sn_of(&frame), expect["sn"].as_u64().unwrap() as u16);
            let f = parse_spec_frame(&pt).expect("spec frame");
            assert_eq!(f.version, 2);
            assert_eq!(f.tid, expect["tid"].as_u64().unwrap() as u16);
            assert_eq!(f.opcode, SpecOpcode::PropertiesChanged);
            assert_eq!(f.objects.len(), 1);
            let o = &f.objects[0];
            assert_eq!(o.siid, expect["siid"].as_u64().unwrap() as u8);
            assert_eq!(o.piid, expect["piid"].as_u64().unwrap() as u16);
            assert_eq!(o.value_type, SpecValueType::Uint32);
            let u32v = u32::from_le_bytes(o.value.clone().try_into().unwrap());
            assert_eq!(format!("{u32v:08x}"), expect["u32"].as_str().unwrap());
        }
        // 端口解码抽查：0x5b090a01 → 9.1V / 0.9A / contract=10 / state=1（LE 字节序）
        let p = PortInfo::from_u32(0x5b090a01);
        assert_eq!(
            (p.voltage_x10v, p.current_x10a, p.contract, p.state),
            (91, 9, 10, 1)
        );
        assert!((p.voltage_v() - 9.1).abs() < 1e-4);
        assert!((p.current_a() - 0.9).abs() < 1e-4);
    }

    #[test]
    fn tx_frame_device_side_roundtrip() {
        // 设备侧用 RX key=okm[16:32]、nonce 前缀 okm[36:40] 解 app 的 TX 帧
        // （镜像 e71.java：RX/TX 各自独立 key 与 nonce 前缀）
        let session = MibleSession {
            okm: core::array::from_fn(|i| i as u8),
        };
        let mut tx = SpecTxState::default();
        let msg = b"hello spec";
        for sn in 0u16..3u16 {
            let frame = tx.encrypt(msg, &session);
            assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), sn);
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&session.okm[36..40]);
            nonce[8..10].copy_from_slice(&sn.to_le_bytes());
            let mut key = [0u8; 16];
            key.copy_from_slice(&session.okm[16..32]);
            let pt = ccm_decrypt(&key, &nonce, &frame[2..]).expect("device-side decrypt");
            assert_eq!(pt, msg);
        }
    }

    #[test]
    fn rx_roll_state_machine() {
        let session = MibleSession {
            okm: core::array::from_fn(|i| i as u8),
        };
        let mut key = [0u8; 16];
        key.copy_from_slice(&session.okm[0..16]);
        // 32768 附近翻 bit15：roll 应 +1（直接构造一帧校验状态机）
        let mut rx2 = SpecRxState { last_sn: 0x7FFF, roll: 5 };
        let mut frame = vec![0x00u8, 0x80]; // sn = 0x8000（bit15 翻转）
        frame.extend_from_slice(&ccm_encrypt(&key, &rx_nonce(&session, 0x8000, 6), b"x"));
        let pt = rx2.decrypt(&frame, &session).expect("roll frame");
        assert_eq!(rx2.roll, 6);
        assert_eq!(pt, b"x");
        // 不翻转（bit15 相同）→ roll 不变
        let mut rx3 = SpecRxState { last_sn: 0x8001, roll: 6 };
        let mut frame = vec![0x02u8, 0x80]; // sn = 0x8002
        frame.extend_from_slice(&ccm_encrypt(&key, &rx_nonce(&session, 0x8002, 6), b"y"));
        let pt = rx3.decrypt(&frame, &session).expect("no-roll frame");
        assert_eq!(rx3.roll, 6);
        assert_eq!(pt, b"y");
    }

    fn sn_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[0], frame[1]])
    }

    /// 与 SpecRxState::decrypt 相同的 RX nonce 构造（测试辅助）。
    fn rx_nonce(session: &MibleSession, sn: u16, roll: u16) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&session.okm[32..36]);
        nonce[8..10].copy_from_slice(&sn.to_le_bytes());
        nonce[10..12].copy_from_slice(&roll.to_le_bytes());
        nonce
    }

    #[test]
    fn port_info_screen_oracle() {
        // 真机三轮对拍（docs §8.3）：C1 屏 9.1V/1.1A → u32=0x5b0b0a01；
        // 缓降后屏 9V/0.9A → u32=0x5b080a01；A 口屏 9V/1.9A → u32=0x5a137001
        let p = PortInfo::from_u32(0x5b0b0a01);
        assert_eq!((p.voltage_x10v, p.current_x10a, p.contract, p.state), (91, 11, 10, 1));
        let p = PortInfo::from_u32(0x5b080a01);
        assert_eq!((p.voltage_v(), p.current_a()), (9.1, 0.8));
        let p = PortInfo::from_u32(0x5a137001);
        assert_eq!((p.voltage_x10v, p.current_x10a), (90, 19));
        assert!((p.power_w() - 9.0 * 1.9).abs() < 1e-4);
    }

    #[test]
    fn spec_tx_frames_golden() {
        // get 帧与实机 probe 手搓帧对拍（ble_probe_mj6.py / ble_native_quick.py，
        // 0x001A 写入被物理层接受 status=0）：get s2.p1001/p1002 + s3.p1003
        let g = build_spec_get(&[(2, 1001), (2, 1002), (3, 1003)], 0x65);
        assert_eq!(g, unhex("0f206500020302e90302ea0303eb03"));
        // 版本交换帧（0x2005 = (2<<12)|5）
        assert_eq!(build_spec_ver(0x64), unhex("05206400f0"));
        // 0x0019 分片流：16B appRandom → CMD start + 1 数据帧，
        // CMD start 与 ble_login.py 实机闭环同字节（0000000b0100）
        let payload: Vec<u8> = (0..16).collect();
        let fs = build_stream_frames(11, &payload);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0], unhex("0000000b0100"));
        let mut expect = vec![0x01u8, 0x00];
        expect.extend_from_slice(&payload);
        assert_eq!(fs[1], expect);
        // 37B payload → 3 帧（18+18+1），n=3
        let long: Vec<u8> = (0..37).collect();
        let fs = build_stream_frames(10, &long);
        assert_eq!(fs.len(), 4);
        assert_eq!(&fs[0][..6], &unhex("0000000a0300")[..]);
        assert_eq!(&fs[3][2..], &long[36..]);
        assert_eq!(build_stream_ack(1), unhex("00000101"));
        assert_eq!(unhex("24000000"), LOGIN_CMD.to_vec());
    }

    #[test]
    fn ht_reading_golden() {
        // 06 实机（2026-11-23）：24.40℃/64%/3.040V
        let h = parse_ebe0ccc1(&unhex("880940e00b")).unwrap();
        assert!((h.temp_c - 24.40).abs() < 1e-3);
        assert!((h.humidity - 64.0).abs() < 1e-3);
        assert!((h.voltage_v - 3.040).abs() < 1e-3);
        // MJWSD05MMC dump 交叉验证：23.10℃/28%/3.225V
        let h = parse_ebe0ccc1(&unhex("06091c990c")).unwrap();
        assert!((h.temp_c - 23.10).abs() < 1e-3);
        assert!((h.humidity - 28.0).abs() < 1e-3);
        assert!((h.voltage_v - 3.225).abs() < 1e-3);
        // 短帧拒绝
        assert!(parse_ebe0ccc1(&unhex("880940")).is_none());
        // 2026-11-24 真机 V4.1 快路径样本：fc08 3b ed0b → 23.00℃/59%/3.053V
        let h = parse_ebe0ccc1(&unhex("fc083bed0b")).unwrap();
        assert!((h.temp_c - 23.00).abs() < 1e-3);
        assert!((h.humidity - 59.0).abs() < 1e-3);
        assert!((h.voltage_v - 3.053).abs() < 1e-3);
        // 负温度自洽（int16 有符号）：f990 → -16.48℃
        let h = parse_ebe0ccc1(&unhex("90f932120c")).unwrap();
        assert!((h.temp_c - (-16.48)).abs() < 1e-3);
        assert_eq!(parse_ebe0ccc4(&unhex("64")), Some(100));
        assert_eq!(parse_ebe0ccc4(&[]), None);
    }
}
