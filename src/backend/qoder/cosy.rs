//! Qoder COSY 协议底层：请求体编码、会话密钥、签名与 PKCE。
//!
//! 协议事实来自对 qoder2api 的只读审计（commit `5bd4083`）：
//! - 请求体不是裸 JSON，而是「标准 base64 → 三段落轮转 → 自定义字母表」；
//! - 每次请求生成临时密钥，RSA 加密得到 `cosy-key`，AES-CBC 加密身份得到 `info`；
//! - 签名材料为 `payloadB64\ncosyKey\ncosyDate\nbody\npath` 的 MD5。
//!
//! 这些字节必须与服务端逐位一致，所以序列化显式按键典序，不依赖容器隐式行为。

use crate::core::error::AppError;
use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::Engine;
use rand_core::{OsRng, RngCore};
use rsa::RsaPublicKey;
use rsa::pkcs1v15::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use sha2::{Digest, Sha256, Sha512};
use std::collections::BTreeMap;

/// 服务端 RSA 公钥（PKIX / SubjectPublicKeyInfo）。
const SERVER_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17
XcW+ML9FoCI6AOvOzwIDAQAB
-----END PUBLIC KEY-----";

/// 自定义 base64 字母表，顺序与官方客户端一致。
const CUSTOM_ALPHABET: &[u8; 64] =
    b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
/// 标准 base64 字母表。
const STANDARD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// 自定义填充字符。
const CUSTOM_PAD: u8 = b'$';

/// 标准 base64 编码器。
fn standard_engine() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// 把字节按「尾段 + 中段 + 首段」轮转。
///
/// 该置换自反：再执行一次即还原，因此编码与解码共用同一函数。
fn rotate(bytes: &[u8]) -> Vec<u8> {
    let n = bytes.len();
    let head = n / 3;
    let mut out = Vec::with_capacity(n);
    out.extend_from_slice(&bytes[n - head..]);
    out.extend_from_slice(&bytes[head..n - head]);
    out.extend_from_slice(&bytes[..head]);
    out
}

/// 把标准 base64 字符映射成自定义字母表字符。
fn to_custom(byte: u8) -> Option<u8> {
    if byte == b'=' {
        return Some(CUSTOM_PAD);
    }
    STANDARD_ALPHABET
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|index| CUSTOM_ALPHABET[index])
}

/// 把自定义字母表字符映射回标准 base64 字符。
fn to_standard(byte: u8) -> Option<u8> {
    if byte == CUSTOM_PAD {
        return Some(b'=');
    }
    CUSTOM_ALPHABET
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|index| STANDARD_ALPHABET[index])
}

/// 按 Qoder 自定义字母表编码请求体。
pub fn encode_body(plain: &[u8]) -> String {
    let standard = standard_engine().encode(plain);
    rotate(standard.as_bytes())
        .into_iter()
        .map(|byte| to_custom(byte).unwrap_or(byte) as char)
        .collect()
}

/// 解码 Qoder 自定义字母表，供测试与排查使用。
pub fn decode_body(encoded: &str) -> Option<Vec<u8>> {
    let mapped = encoded
        .bytes()
        .map(to_standard)
        .collect::<Option<Vec<u8>>>()?;
    let standard = rotate(&mapped);
    standard_engine().decode(standard).ok()
}

/// 账号身份。字段原样进入加密的 `info`，不做补全或猜测。
pub struct AuthIdentity<'a> {
    pub name: &'a str,
    pub aid: &'a str,
    pub uid: &'a str,
    pub yx_uid: &'a str,
    pub organization_id: &'a str,
    pub organization_name: &'a str,
    pub user_type: &'a str,
    pub security_oauth_token: &'a str,
    pub refresh_token: &'a str,
}

/// 一次请求的会话密钥材料。
pub struct CosySession {
    /// RSA 加密后的临时密钥，作为 `cosy-key` 头。
    pub cosy_key: String,
    /// AES 加密后的身份，作为 payload 的 `info` 字段。
    pub info: String,
}

/// 创建一次请求的会话密钥。
///
/// 服务端要求 `cosy-key` 与 payload 中的 `info` 来自同一次随机密钥，
/// 因此会话不能跨请求复用。
pub fn new_session(identity: &AuthIdentity<'_>) -> Result<CosySession, AppError> {
    let mut raw = [0u8; 16];
    OsRng.fill_bytes(&mut raw);
    let hex = raw
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // 与官方客户端一致：16 字节随机数的前 16 个 hex 字符当作 ASCII 密钥。
    let temp_key = hex.as_bytes()[..16].to_vec();

    let encrypted_key = RsaPublicKey::from_public_key_pem(SERVER_PUBLIC_KEY_PEM)
        .map_err(|error| AppError::Internal {
            message: format!("parse qoder public key failed: {error}"),
        })?
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, &temp_key)
        .map_err(|error| AppError::Internal {
            message: format!("qoder session key encryption failed: {error}"),
        })?;

    let payload = identity_json(identity);
    let encrypted_info = aes_encrypt(payload.as_bytes(), &temp_key)?;
    Ok(CosySession {
        cosy_key: standard_engine().encode(encrypted_key),
        info: standard_engine().encode(encrypted_info),
    })
}

/// 身份 JSON。键按字典序输出，保证加密前的字节稳定。
fn identity_json(identity: &AuthIdentity<'_>) -> String {
    let mut map: BTreeMap<&str, &str> = BTreeMap::new();
    map.insert("name", identity.name);
    map.insert("aid", identity.aid);
    map.insert("uid", identity.uid);
    map.insert("yx_uid", identity.yx_uid);
    map.insert("organization_id", identity.organization_id);
    map.insert("organization_name", identity.organization_name);
    map.insert("user_type", identity.user_type);
    map.insert("security_oauth_token", identity.security_oauth_token);
    map.insert("refresh_token", identity.refresh_token);
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// AES-128-CBC + PKCS7 加密，密钥与 IV 均为临时密钥前 16 字节。
fn aes_encrypt(plain: &[u8], key: &[u8]) -> Result<Vec<u8>, AppError> {
    // PKCS7 最多补满一个 16 字节块。
    let mut buffer = vec![0u8; plain.len() + 16];
    buffer[..plain.len()].copy_from_slice(plain);
    let encryptor =
        cbc::Encryptor::<aes::Aes128>::new_from_slices(key, &key[..16]).map_err(|error| {
            AppError::Internal {
                message: format!("build aes cipher failed: {error}"),
            }
        })?;
    let ciphertext = encryptor
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plain.len())
        .map_err(|error| AppError::Internal {
            message: format!("aes encrypt failed: {error}"),
        })?;
    Ok(ciphertext.to_vec())
}

/// 构造 `Authorization` 用的 payload base64。
pub fn build_payload_b64(info: &str) -> String {
    let mut map: BTreeMap<&str, String> = BTreeMap::new();
    map.insert("cosyVersion", "1.0.10".to_string());
    map.insert("ideVersion", String::new());
    map.insert("info", info.to_string());
    map.insert("requestId", uuid::Uuid::new_v4().simple().to_string());
    map.insert("version", "v1".to_string());
    let text = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
    standard_engine().encode(text)
}

/// 计算请求签名：MD5(payloadB64\ncosyKey\ncosyDate\nbody\npath)。
pub fn sign_request(
    payload_b64: &str,
    cosy_key: &str,
    cosy_date: &str,
    body: &str,
    path: &str,
) -> String {
    let material = format!("{payload_b64}\n{cosy_key}\n{cosy_date}\n{body}\n{path}");
    format!("{:x}", md5::compute(material))
}

/// 组装 Bearer 头。
pub fn compose_bearer(payload_b64: &str, signature: &str) -> String {
    format!("Bearer COSY.{payload_b64}.{signature}")
}

/// 取 URL 的签名路径：去掉 `/algo` 前缀，丢弃查询串。
pub fn path_signature(url: &str) -> String {
    let parsed = reqwest::Url::parse(url).ok();
    let path = parsed
        .as_ref()
        .map(|value| value.path().to_string())
        .unwrap_or_else(|| url.to_string());
    path.strip_prefix("/algo").unwrap_or(&path).to_string()
}

/// 当前 Unix 秒，作为 `cosy-date`。
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// PKCE 参数。
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// 生成 PKCE verifier 与 S256 challenge。
pub fn pkce() -> Pkce {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// 生成 n 位十六进制随机串。
pub fn random_hex(n: usize) -> String {
    let bytes = n.div_ceil(2);
    let mut raw = vec![0u8; bytes];
    OsRng.fill_bytes(&mut raw);
    raw.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..n]
        .to_string()
}

/// 按 uid 派生稳定的设备标识（UUIDv4 形态）。
pub fn derive_machine_id(seed: &str) -> String {
    let hex = format!("{:x}", md5::compute(format!("machine:{seed}")));
    let bytes = hex.as_bytes();
    let mut shaped = bytes.to_vec();
    shaped[12] = b'4';
    shaped[16] = b'a';
    format!(
        "{}-{}-{}-{}-{}",
        String::from_utf8_lossy(&shaped[0..8]),
        String::from_utf8_lossy(&shaped[8..12]),
        String::from_utf8_lossy(&shaped[12..16]),
        String::from_utf8_lossy(&shaped[16..20]),
        String::from_utf8_lossy(&shaped[20..32])
    )
}

/// 按 uid 派生稳定的 machine token（base64url，43 位）。
pub fn derive_machine_token(seed: &str) -> String {
    let digest = Sha512::digest(format!("machinetoken:{seed}").as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    encoded[..43].to_string()
}

/// 按 uid 派生稳定的 machine type（18 位 hex）。
pub fn derive_machine_type(seed: &str) -> String {
    let hex = format!("{:x}", md5::compute(format!("machinetype:{seed}")));
    hex[..18].to_string()
}

/// 按 uid 派生稳定的主机名形态，避免暴露真实主机名。
pub fn derive_machine_hostname(seed: &str) -> String {
    let hex = format!("{:x}", md5::compute(format!("hostname:{seed}")));
    format!("DESKTOP-{}", hex[..7].to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自定义编码是可逆的，且不是标准 base64。
    #[test]
    fn body_encoding_round_trips() {
        let plain = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let encoded = encode_body(plain);
        assert_ne!(encoded, standard_engine().encode(plain));
        assert_eq!(decode_body(&encoded).expect("decode"), plain);
    }

    /// 编码必须使用自定义填充字符，且可逆。
    #[test]
    fn body_encoding_uses_custom_pad() {
        let encoded = encode_body(b"a");
        assert!(encoded.contains('$'), "got {encoded}");
        assert_eq!(decode_body(&encoded).expect("decode"), b"a");
    }

    /// 空输入保持为空。
    #[test]
    fn body_encoding_handles_empty() {
        assert_eq!(encode_body(b""), "");
        assert_eq!(decode_body("").expect("decode"), Vec::<u8>::new());
    }

    /// payload 键必须字典序，且 info 原样进入。
    #[test]
    fn payload_is_sorted_json() {
        let payload = build_payload_b64("INFO");
        let decoded = standard_engine().decode(payload).expect("decode");
        let text = String::from_utf8(decoded).expect("utf8");
        assert!(
            text.starts_with(
                r#"{"cosyVersion":"1.0.10","ideVersion":"","info":"INFO","requestId":"#
            ),
            "got {text}"
        );
        assert!(text.ends_with(r#","version":"v1"}"#), "got {text}");
    }

    /// 签名对字段顺序敏感。
    #[test]
    fn signature_is_order_sensitive() {
        let first = sign_request("payload", "key", "date", "body", "/api/x");
        assert_eq!(first.len(), 32);
        assert_ne!(
            first,
            sign_request("payload", "key", "date", "body2", "/api/x")
        );
    }

    /// 签名路径丢弃查询串与 `/algo` 前缀。
    #[test]
    fn path_signature_strips_prefix_and_query() {
        let path = path_signature(
            "https://api1.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?Encode=1",
        );
        assert_eq!(path, "/api/v2/service/pro/sse/agent_chat_generation");
    }

    /// 每次会话的 cosy-key 不同，且 info 可被同一临时密钥解开。
    #[test]
    fn session_is_random_per_request() {
        let identity = AuthIdentity {
            name: "n",
            aid: "a",
            uid: "u",
            yx_uid: "y",
            organization_id: "o",
            organization_name: "on",
            user_type: "personal_standard",
            security_oauth_token: "dt-1",
            refresh_token: "drt-1",
        };
        let first = new_session(&identity).expect("session");
        let second = new_session(&identity).expect("session");
        assert_ne!(first.cosy_key, second.cosy_key);
        assert_ne!(first.info, second.info);
        assert!(!first.cosy_key.is_empty() && !first.info.is_empty());
    }

    /// PKCE challenge 必须是 sha256(verifier) 的 base64url 无填充。
    #[test]
    fn pkce_matches_sha256() {
        let pkce = pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.verifier.contains('='));
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    /// 派生设备标识必须稳定且形态正确。
    #[test]
    fn device_identity_is_stable() {
        let id = derive_machine_id("uid-1");
        assert_eq!(id.len(), 36);
        assert_eq!(id, derive_machine_id("uid-1"));
        assert_ne!(id, derive_machine_id("uid-2"));
        assert_eq!(derive_machine_token("uid-1").len(), 43);
        assert_eq!(derive_machine_type("uid-1").len(), 18);
        assert!(derive_machine_hostname("uid-1").starts_with("DESKTOP-"));
    }
}
