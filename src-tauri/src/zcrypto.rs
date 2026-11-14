
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

pub const PREFIX: &str = "enc:v1:";

pub fn node_platform_for<'a>(os: &'a str) -> &'a str {
    match os {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn node_os() -> &'static str {
    node_platform_for(std::env::consts::OS)
}

fn pick_username(username: Option<&str>, user: Option<&str>, logname: Option<&str>) -> String {
    username
        .or(user)
        .or(logname)
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(not(windows))]
fn passwd_username() -> Option<String> {
    let out = std::process::Command::new("id")
        .arg("-un")
        .output()
        .ok()?;
    let n = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!n.is_empty()).then_some(n)
}

fn compose_fallback_secret(platform: &str, home: &str, username: &str) -> String {
    format!("zcode-credential-fallback:{}:{}:{}", platform, home, username)
}

pub fn default_secret(home: &Path) -> String {
    if let Ok(s) = std::env::var("ZCODE_CREDENTIAL_SECRET") {
        return s;
    }
    #[cfg(windows)]
    let primary = std::env::var("USERNAME").ok();
    #[cfg(not(windows))]
    let primary = passwd_username().or_else(|| std::env::var("USER").ok());
    let username = pick_username(
        primary.as_deref(),
        std::env::var("USER").ok().as_deref(),
        std::env::var("LOGNAME").ok().as_deref(),
    );
    compose_fallback_secret(node_os(), &home.display().to_string(), &username)
}

fn derive_key(secret: &str) -> [u8; 32] {
    let d = Sha256::digest(secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

pub fn is_encrypted(v: &str) -> bool {
    v.starts_with(PREFIX)
}

pub fn decrypt_with_secret(value: &str, secret: &str) -> Result<String, String> {
    let body = value.strip_prefix(PREFIX).ok_or("不是 enc:v1 格式")?;
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() != 3 {
        return Err("enc:v1 格式不正确".into());
    }
    let nonce_b = URL_SAFE_NO_PAD.decode(parts[0]).map_err(|e| format!("nonce 解码失败：{e}"))?;
    let tag_b = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|e| format!("tag 解码失败：{e}"))?;
    let ct_b = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|e| format!("密文解码失败：{e}"))?;
    if nonce_b.len() != 12 {
        return Err("nonce 长度异常".into());
    }
    let key = derive_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败：{e}"))?;
    let mut buf = ct_b.clone();
    buf.extend_from_slice(&tag_b);
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce_b), buf.as_slice())
        .map_err(|_| "解密失败（密钥不匹配或数据损坏）".to_string())?;
    Ok(String::from_utf8_lossy(&pt).to_string())
}

pub fn decrypt_json_opt(value: Option<&str>, secret: &str) -> Option<Value> {
    let v = value?;
    let plain = decrypt_plain(v, secret)?;
    serde_json::from_str(&plain).ok()
}

pub fn decode_jwt(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    if payload.is_empty() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn jwt_exp_ms(jwt: &str) -> Option<i64> {
    let p = decode_jwt(jwt)?;
    let exp = p.get("exp")?;
    exp.as_i64().or_else(|| exp.as_f64().map(|f| f as i64))
        .map(|s| s.saturating_mul(1000))
}

// ---------------------------------------------------------------- enc:v2:os:（ZCode 3.18）

/// ZCode 3.18 起凭据换成 `enc:v2:os:<指纹>:<nonce>.<tag>.<ct>`：
/// 32 字节 AES-256-GCM 密钥存在**系统凭据库**（Windows Credential Manager，条目名
/// `<hash>.ZCode Credential Encryption`），指纹 = sha256(密钥) 的前 24 个 hex 字符。
/// 解不开就等于「不知道这个窗口登录的是谁」，界面只能拿启动时记的账号去猜 ——
/// 用户就会看到「使用中」标在一个自己没在用的号上。
#[cfg(windows)]
mod keyring {
    use std::ffi::c_void;

    #[repr(C)]
    struct Credential {
        flags: u32,
        cred_type: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: u64,
        blob_size: u32,
        blob: *mut u8,
        persist: u32,
        attr_count: u32,
        attributes: *mut c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredEnumerateW(
            filter: *const u16,
            flags: u32,
            count: *mut u32,
            creds: *mut *mut *mut Credential,
        ) -> i32;
        fn CredFree(buf: *mut c_void);
    }

    const CRED_TYPE_GENERIC: u32 = 1;

    /// 读出所有 ZCode 凭据密钥 → `[(指纹, 32 字节密钥)]`
    pub(super) fn zcode_keys() -> Vec<(String, [u8; 32])> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let mut out = Vec::new();
        let mut count: u32 = 0;
        let mut arr: *mut *mut Credential = std::ptr::null_mut();
        unsafe {
            // 过滤串是**前缀**匹配，而 ZCode 的条目名是 `<hash>.ZCode Credential Encryption`，
            // 只能全量枚举再自己按后缀筛。
            if CredEnumerateW(std::ptr::null(), 0, &mut count, &mut arr) == 0 || arr.is_null() {
                return out;
            }
            for i in 0..count as usize {
                let p = *arr.add(i);
                if p.is_null() {
                    continue;
                }
                let c = &*p;
                if c.cred_type != CRED_TYPE_GENERIC
                    || c.blob.is_null()
                    || c.blob_size == 0
                    || c.target_name.is_null()
                {
                    continue;
                }
                let mut name_len = 0usize;
                while *c.target_name.add(name_len) != 0 {
                    name_len += 1;
                }
                let name: Vec<u16> =
                    std::slice::from_raw_parts(c.target_name, name_len).to_vec();
                let name = String::from_utf16_lossy(&name);
                if !name.to_ascii_lowercase().ends_with(".zcode credential encryption") {
                    continue;
                }
                let raw: Vec<u8> = std::slice::from_raw_parts(c.blob, c.blob_size as usize).to_vec();
                if raw.len() % 2 != 0 {
                    continue;
                }
                let text: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let text = String::from_utf16_lossy(&text);
                let Ok(key) = URL_SAFE_NO_PAD.decode(text.trim()) else {
                    continue;
                };
                if key.len() != 32 {
                    continue;
                }
                let mut k = [0u8; 32];
                k.copy_from_slice(&key);
                let fp: String = super::sha256_hex(&k).chars().take(24).collect();
                out.push((fp, k));
            }
            CredFree(arr as *mut c_void);
        }
        out
    }
}

#[cfg(not(windows))]
mod keyring {
    pub(super) fn zcode_keys() -> Vec<(String, [u8; 32])> {
        Vec::new()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 密钥只读一次（系统凭据库里的密钥不轮换，每次认人都枚举一遍太亏）。
fn v2_keys() -> &'static Vec<(String, [u8; 32])> {
    static CACHE: std::sync::OnceLock<Vec<(String, [u8; 32])>> = std::sync::OnceLock::new();
    CACHE.get_or_init(keyring::zcode_keys)
}

/// 解 `enc:v2:os:` 载荷。载荷结构与 v1 一致（nonce / tag / 密文三段），差别只在密钥来源。
pub fn decrypt_v2(value: &str) -> Option<String> {
    let body = value.strip_prefix("enc:v2:os:")?;
    let (fingerprint, rest) = body.split_once(':')?;
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let key = v2_keys()
        .iter()
        .find(|(fp, _)| fp.eq_ignore_ascii_case(fingerprint))
        .map(|(_, k)| *k)?;
    let nonce_b = URL_SAFE_NO_PAD.decode(parts[0]).ok()?;
    let tag_b = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let mut buf = URL_SAFE_NO_PAD.decode(parts[2]).ok()?;
    if nonce_b.len() != 12 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    buf.extend_from_slice(&tag_b);
    let pt = cipher.decrypt(Nonce::from_slice(&nonce_b), buf.as_slice()).ok()?;
    Some(String::from_utf8_lossy(&pt).to_string())
}

/// 统一入口：`enc:v2:os:` 走系统凭据库密钥，`enc:v1:` 走历史 fallback 密钥。
fn decrypt_plain(value: &str, secret: &str) -> Option<String> {
    if value.starts_with("enc:v2:os:") {
        if let Some(p) = decrypt_v2(value) {
            return Some(p);
        }
    }
    if value.starts_with(PREFIX) {
        return decrypt_with_secret(value, secret).ok();
    }
    if value.starts_with("enc:v") {
        return None;
    }
    Some(value.to_string())
}


#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Identity {
    pub provider: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub user_id: Option<String>,
}

impl Identity {
    pub fn label(&self) -> Option<String> {
        self.display_name
            .clone()
            .or_else(|| self.username.clone())
            .or_else(|| self.email.clone())
    }
}

pub fn identity_with_secret(creds: &Value, secret: &str) -> Identity {
    let mut id = Identity { provider: "bigmodel".into(), ..Default::default() };
    let map = match creds.as_object() {
        Some(m) => m,
        None => return id,
    };
    if let Some(ap) = map.get("oauth:active_provider").and_then(|v| v.as_str()) {
        let plain = decrypt_plain(ap, secret);
        if let Some(p) = plain.filter(|p| !p.is_empty()) {
            id.provider = p;
        }
    }
    let ui_key = format!("oauth:{}:user_info", id.provider);
    let ui = map
        .get(ui_key.as_str())
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_json_opt(Some(v), secret));
    if let Some(ui) = ui {
        id.username = ui.get("username").and_then(|v| v.as_str()).map(String::from);
        id.display_name = ui.get("displayName").and_then(|v| v.as_str()).map(String::from);
        id.email = ui
            .get("email")
            .and_then(|v| v.as_str())
            .or_else(|| ui.get("rawProfile").and_then(|r| r.get("email")).and_then(|v| v.as_str()))
            .map(String::from);
        if let Some(uid) = ui.get("id").filter(|v| !v.is_null()) {
            id.user_id = uid.as_str().map(String::from).or_else(|| serde_json::to_string(uid).ok());
        }
    }
    if id.user_id.is_none() {
        let at_key = format!("oauth:{}:access_token", id.provider);
        if let Some(at) = map.get(at_key.as_str()).and_then(|v| v.as_str()) {
            let plain = decrypt_plain(at, secret);
            if let Some(jwt) = plain.as_deref().and_then(decode_jwt) {
                id.user_id = jwt
                    .get("user_id")
                    .or_else(|| jwt.get("sub"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
        }
    }
    id
}

pub fn account_identity(creds: &Value, home: &Path) -> Identity {
    identity_with_secret(creds, &default_secret(home))
}
