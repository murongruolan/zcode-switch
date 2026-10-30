
use crate::quota;
use crate::zcrypto;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const BILLING_PREVIEW_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/preview";
pub const BILLING_CLAIM_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/claim";
pub const BILLING_BALANCE_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/balance";
pub const CLIENT_CONFIGS_URL: &str = "https://zcode.z.ai/api/v1/client/configs";

const CLAIM_TIMEOUT_SECS: u64 = 25;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimGrant {
    pub name: String,
    pub units: f64,
    pub period: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimPlan {
    pub plan_id: String,
    pub name: String,
    pub description: String,
    pub priority: i64,
    pub grants: Vec<String>,
    #[serde(default)]
    pub grant_items: Vec<ClaimGrant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptchaConfig {
    pub enabled: bool,
    pub region: String,
    pub prefix: String,
    pub scene_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimOutcome {
    pub account_id: String,
    pub account_name: String,
    pub plan_name: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub server_time: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ClaimError {
    pub code: i64,
    pub message: String,
    pub next_at: Option<i64>,
}

impl From<String> for ClaimError {
    fn from(message: String) -> Self {
        ClaimError {
            code: -1,
            message,
            next_at: None,
        }
    }
}

fn claim_error(code: i64, body: &Value) -> ClaimError {
    let next_at = if code == 1005 {
        body.pointer("/data/plan/ends_at")
            .and_then(|x| x.as_i64())
            .map(|s| s * 1000)
    } else {
        None
    };
    ClaimError {
        code,
        message: failure_message(code, body),
        next_at,
    }
}

pub fn failure_payload(
    account_id: &str,
    account_name: &str,
    plan_name: &str,
    err: &ClaimError,
) -> Value {
    serde_json::json!({
        "ok": false,
        "accountId": account_id,
        "accountName": account_name,
        "planName": plan_name,
        "code": err.code,
        "nextAt": err.next_at,
        "message": err.message,
    })
}

fn claim_token(creds: &Value, config: Option<&Value>, secret: &str) -> Result<String, String> {
    let jwt = creds
        .get("zcodejwttoken")
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_credential(v, secret));
    if let Some(t) = jwt.filter(|t| t.trim().len() > 20) {
        return Ok(t);
    }
    if let Some(t) = quota::zai_billing_token(creds, config, secret) {
        return Ok(t);
    }
    Err(crate::i18n::tr("err.claim.no_jwt"))
}

fn decrypt_credential(v: &str, secret: &str) -> Option<String> {
    if zcrypto::is_encrypted(v) {
        zcrypto::decrypt_with_secret(v, secret).ok()
    } else {
        Some(v.to_string())
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(CLAIM_TIMEOUT_SECS))
        .build()
}

pub const EVENT_REPORT_URL: &str = "https://zcode.z.ai/api/v1/event/report";
pub const ACTIVATION_EVENTS: [&str; 2] = ["app_launch", "app_daily_active"];
const SCREEN_RESOLUTION: &str = "2560x1440";
const ACTIVATION_TIMEOUT_SECS: u64 = 10;

pub(crate) fn telemetry_user_id(home: &Path, creds: &Value) -> Option<String> {
    let secret = zcrypto::default_secret(home);
    let provider = creds
        .get("oauth:active_provider")
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_credential(v, &secret))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "zai".to_string());
    let raw = creds
        .get(format!("oauth:{provider}:user_info"))?
        .as_str()?;
    let plain = decrypt_credential(raw, &secret)?;
    let info: Value = serde_json::from_str(&plain).ok()?;
    let pick = |k: &str| {
        info.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    pick("id").or_else(|| pick("user_id")).map(String::from)
}

fn activation_event_body(element: &str, event_id: &str, user_id: &str, mid: &str) -> Value {
    serde_json::json!({
        "event_id": event_id,
        "client_timezone": quota::client_timezone(),
        "client_language": quota::ZCODE_LANG,
        "element_name": element,
        "event_region": "app",
        "event_type": "view",
        "event_text": "",
        "event_extra_detail": {},
        "user_id": user_id,
        "screen_resolution": SCREEN_RESOLUTION,
        "app_version": quota::zcode_app_version(),
        "device_os_category": device_os_category(),
        "device_os_version": quota::os_version().unwrap_or_default(),
        "device_mid": mid,
        "mac_id": "",
        "marketing_params": "{}",
    })
}

fn device_os_category() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

pub fn report_activation_events(user_id: &str, device_mid: &str) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(ACTIVATION_TIMEOUT_SECS))
        .build();
    for element in ACTIVATION_EVENTS {
        let body = activation_event_body(
            element,
            &uuid::Uuid::new_v4().to_string(),
            user_id,
            device_mid,
        );
        let resp = agent
            .post(EVENT_REPORT_URL)
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(|e| {
                crate::i18n::trf("err.claim.activate_req", &[("e", http_err("", e).trim())])
            })?
            .into_string()
            .map_err(|e| crate::i18n::trf("err.claim.activate_req", &[("e", &e.to_string())]))?;
        let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(failure_message(code, &v));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// preview 空时的 plan_id 兜底缓存。
//
// 背景：服务端 /billing/preview 会返回 `{"code":0,"msg":""}`（没有 data），但同一
// 账号、同一个 plan_id 去 POST /billing/claim 却可能成功。ZCode 客户端之所以还能
// 显示“领取”，是因为它把上一次见到的 plan_id 留在本地缓存里，preview 空了照样拿
// 缓存去 claim。z-switch 之前只认 preview，于是显示“没有可领取”，也就没有入口。
// 这里补上同样的缓存层：preview 有数据就写入缓存，preview 空就回退到缓存。
//   ZSW_CLAIM_FROM_CACHE=0  关闭兜底（对照实验用）
//   ZSW_NO_PLAN_CACHE=1     不写缓存
// ---------------------------------------------------------------------------
const PLAN_CACHE_FILE: &str = "claim-plan-cache.json";
const PLAN_CACHE_TTL_SECS: i64 = 7 * 24 * 3600;

fn plan_cache_path(home: &Path) -> std::path::PathBuf {
    home.join(".zcode-switch").join(PLAN_CACHE_FILE)
}

fn read_plan_cache(home: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(plan_cache_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_plan_cache(home: &Path, account_id: &str, plans: &[ClaimPlan]) {
    if std::env::var("ZSW_NO_PLAN_CACHE").map(|v| v == "1").unwrap_or(false) {
        return;
    }
    let mut map = read_plan_cache(home);
    map.insert(
        account_id.to_string(),
        serde_json::json!({
            "saved_at": chrono::Utc::now().timestamp(),
            "plans": plans,
        }),
    );
    let path = plan_cache_path(home);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(map)) {
        let _ = std::fs::write(path, text);
    }
}

fn cached_plans(home: &Path, account_id: &str) -> Vec<ClaimPlan> {
    let map = read_plan_cache(home);
    let Some(entry) = map.get(account_id) else {
        return vec![];
    };
    let saved_at = entry.get("saved_at").and_then(|v| v.as_i64()).unwrap_or(0);
    if saved_at > 0 && chrono::Utc::now().timestamp() - saved_at > PLAN_CACHE_TTL_SECS {
        return vec![];
    }
    entry
        .get("plans")
        .and_then(|v| serde_json::from_value::<Vec<ClaimPlan>>(v.clone()).ok())
        .unwrap_or_default()
}

/// balance 里已存在的 plan_id = 这个账号“已经拿到过”的套餐（领取后才会出现在 balance）。
/// 返回 None 表示没问到（网络/接口失败，重试一次仍失败），此时不做任何猜测。
fn balance_snapshot(token: &str, mid: Option<String>) -> Option<(Vec<String>, Vec<ClaimPlan>)> {
    let url = format!(
        "{BILLING_BALANCE_URL}?app_version={}&platform={}",
        quota::zcode_app_version(),
        quota::client_platform()
    );
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(700));
        }
        if let Ok(plans) = preview_once(&url, token, mid.clone()) {
            let ids = plans.iter().map(|p| p.plan_id.clone()).collect();
            return Some((ids, plans));
        }
    }
    None
}

/// 把已经拥有的 plan_id 从缓存里剔掉，避免“领过的还挂在缓存里”反复出现。
fn prune_plan_cache(home: &Path, account_id: &str, owned: &[String]) {
    if owned.is_empty() {
        return;
    }
    let mut map = read_plan_cache(home);
    let Some(entry) = map.get(account_id).cloned() else {
        return;
    };
    let plans: Vec<ClaimPlan> = entry
        .get("plans")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let kept: Vec<ClaimPlan> = plans
        .into_iter()
        .filter(|p| !owned.iter().any(|id| id == &p.plan_id))
        .collect();
    if kept.is_empty() {
        map.remove(account_id);
    } else {
        map.insert(
            account_id.to_string(),
            serde_json::json!({
                "saved_at": chrono::Utc::now().timestamp(),
                "plans": kept,
            }),
        );
    }
    let path = plan_cache_path(home);
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(map)) {
        let _ = std::fs::write(path, text);
    }
}

/// 活动池：记录“这台机器见过的活动 plan_id”，与账号无关。
/// 领取过的账号会在 per-account 缓存里被剪掉，但活动本身仍然存在——
/// 新登录的号要靠这个池子才知道“现在有哪个活动可领”。
const ACTIVITY_KEY: &str = "__activity";
const ACTIVITY_TTL_SECS: i64 = 36 * 3600;

fn write_plan_cache(home: &Path, map: serde_json::Map<String, Value>) {
    let path = plan_cache_path(home);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(map)) {
        let _ = std::fs::write(path, text);
    }
}

/// 活动池的一条记录。plan_id 是活动级标识（例：zcode-v3-start-plan-1030-wk），
/// 同一个活动对所有账号通用，所以它可以跨账号复用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub plan_id: String,
    pub name: String,
    pub first_seen: i64,
    pub last_seen: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<ClaimPlan>,
}

/// 兼容旧格式（`{saved_at, plans:[ClaimPlan]}`）并读出 history。
fn read_activity(home: &Path) -> Vec<ActivityEntry> {
    let map = read_plan_cache(home);
    let Some(entry) = map.get(ACTIVITY_KEY) else {
        return vec![];
    };
    if let Some(history) = entry
        .get("history")
        .and_then(|v| serde_json::from_value::<Vec<ActivityEntry>>(v.clone()).ok())
    {
        return history;
    }
    // 旧格式迁移：把 plans 数组折成 history。
    let saved_at = entry.get("saved_at").and_then(|v| v.as_i64()).unwrap_or(0);
    entry
        .get("plans")
        .and_then(|v| serde_json::from_value::<Vec<ClaimPlan>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|p| ActivityEntry {
            plan_id: p.plan_id.clone(),
            name: p.name.clone(),
            first_seen: saved_at,
            last_seen: saved_at,
            plan: Some(p),
        })
        .collect()
}

fn write_activity(home: &Path, history: &[ActivityEntry]) {
    let mut map = read_plan_cache(home);
    map.insert(
        ACTIVITY_KEY.to_string(),
        serde_json::json!({
            "saved_at": chrono::Utc::now().timestamp(),
            "history": history,
        }),
    );
    write_plan_cache(home, map);
}

fn save_activity(home: &Path, plans: &[ClaimPlan]) {
    if plans.is_empty() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let mut history = read_activity(home);
    for p in plans {
        if let Some(e) = history.iter_mut().find(|e| e.plan_id == p.plan_id) {
            e.last_seen = now;
            e.name = p.name.clone();
            e.plan = Some(p.clone());
        } else {
            history.push(ActivityEntry {
                plan_id: p.plan_id.clone(),
                name: p.name.clone(),
                first_seen: now,
                last_seen: now,
                plan: Some(p.clone()),
            });
        }
    }
    write_activity(home, &history);
}

/// 活动历史，最近见到的排前面（给 UI 显示「当前 / 上次」用）。
pub fn activity_history(home: &Path) -> Vec<ActivityEntry> {
    let mut history = read_activity(home);
    history.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    history
}

/// 仍在有效期内的活动，用于给账号生成可领候选。
fn activity_plans(home: &Path) -> Vec<ClaimPlan> {
    let now = chrono::Utc::now().timestamp();
    read_activity(home)
        .into_iter()
        .filter(|e| e.last_seen <= 0 || now - e.last_seen <= ACTIVITY_TTL_SECS)
        .filter_map(|e| e.plan)
        .collect()
}

/// 同一个活动对所有账号用的是同一个 plan_id（例：zcode-v3-start-plan-1030-wk）。
/// 所以别的账号见过的活动，也应当能当这个账号的候选——否则新登录的号永远看不到
/// 这种“按活动发、而不是按账号发”的额度。
fn candidate_plans(home: &Path, account_id: &str) -> Vec<ClaimPlan> {
    let mut out = cached_plans(home, account_id);
    for p in activity_plans(home) {
        if !out.iter().any(|c| c.plan_id == p.plan_id) {
            out.push(p);
        }
    }
    let map = read_plan_cache(home);
    for (key, entry) in map.iter() {
        if key == account_id {
            continue;
        }
        let saved_at = entry.get("saved_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if saved_at > 0 && chrono::Utc::now().timestamp() - saved_at > PLAN_CACHE_TTL_SECS {
            continue;
        }
        if let Some(plans) = entry
            .get("plans")
            .and_then(|v| serde_json::from_value::<Vec<ClaimPlan>>(v.clone()).ok())
        {
            for p in plans {
                if !out.iter().any(|c| c.plan_id == p.plan_id) {
                    out.push(p);
                }
            }
        }
    }
    out
}

pub fn preview_plans(
    home: &Path,
    account_id: &str,
    creds: &Value,
    config: Option<&Value>,
    device_mid: Option<String>,
) -> Result<Vec<ClaimPlan>, String> {
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let url = format!(
        "{BILLING_PREVIEW_URL}?app_version={}&platform={}",
        quota::zcode_app_version(),
        quota::client_platform()
    );
    let mid_for_balance = device_mid.clone();
    let preview = preview_once(&url, &token, device_mid);

    // 1) preview 有数据 = 服务端确认可领，直接用（不再多打一次 balance）。
    if let Ok(plans) = &preview {
        if !plans.is_empty() {
            save_plan_cache(home, account_id, plans);
            save_activity(home, plans);
            crate::flowlog::log(
                "claim",
                "preview",
                &format!("account={account_id} plans={}", plans.len()),
            );
            return Ok(plans.clone());
        }
        crate::flowlog::log("claim", "preview-empty", &format!("account={account_id}"));
    } else if let Err(e) = &preview {
        crate::flowlog::log(
            "claim",
            "preview-err",
            &format!("account={account_id} {}", e.trim()),
        );
    }
    if std::env::var("ZSW_CLAIM_FROM_CACHE").map(|v| v == "0").unwrap_or(false) {
        return preview;
    }

    // 2) preview 空：用 balance 判断“这个 plan 是不是已经领过了”。
    //    balance 里有的 plan_id = 已领取，绝不显示；只有“见过但还没拥有”的才显示。
    let Some((owned_ids, owned_plans)) = balance_snapshot(&token, mid_for_balance) else {
        crate::flowlog::log(
            "claim",
            "cache-skip",
            &format!("account={account_id} reason=balance-unavailable"),
        );
        return preview;
    };
    // 把本次看到的活动记进活动池：即使这个号已经领过，别的号也该知道有这回事。
    save_activity(home, &owned_plans);
    prune_plan_cache(home, account_id, &owned_ids);
    let candidates = candidate_plans(home, account_id);
    let cand_n = candidates.len();
    let fresh: Vec<ClaimPlan> = candidates
        .into_iter()
        .filter(|p| !owned_ids.iter().any(|id| id == &p.plan_id))
        .collect();
    if fresh.is_empty() {
        crate::flowlog::log(
            "claim",
            "cache-none",
            &format!("account={account_id} candidates={cand_n} owned={}", owned_ids.len()),
        );
        return preview;
    }
    crate::flowlog::log(
        "claim",
        "preview-cache",
        &format!(
            "account={account_id} plans={} owned={}",
            fresh.len(),
            owned_ids.len()
        ),
    );
    Ok(fresh)
}

fn http_err(prefix: &str, e: ureq::Error) -> String {
    if let ureq::Error::Status(code, resp) = e {
        let body = resp.into_string().unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| ["msg", "message", "error"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(String::from)))
            .unwrap_or_default();
        return format!("{prefix} HTTP {code}: {msg}");
    }
    format!("{prefix} {e}")
}

fn preview_once(url: &str, token: &str, mid: Option<String>) -> Result<Vec<ClaimPlan>, String> {
    let mut req = agent().get(url);
    for (k, v) in quota::zai_billing_headers_with_mid(token, mid) {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| http_err(&crate::i18n::tr("err.claim.preview_req"), e))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(failure_message(code, &v));
    }
    let plans = v
        .pointer("/data/plans")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ClaimPlan> = plans.iter().filter_map(parse_plan).collect();
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.plan_id.cmp(&b.plan_id)));
    Ok(out)
}

pub fn submit_claim(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    plan_id: &str,
    captcha_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<String>,
) -> Result<Value, ClaimError> {
    if captcha_param.trim().is_empty() {
        return Err(crate::i18n::tr("err.claim.no_captcha").into());
    }
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let mut req = agent().post(BILLING_CLAIM_URL);
    for (k, v) in quota::zai_billing_headers_with_mid(&token, device_mid) {
        req = req.set(&k, &v);
    }
    req = req.set("X-Aliyun-Captcha-Verify-Param", captcha_param.trim());
    if let Some(r) = captcha_region.filter(|r| !r.trim().is_empty()) {
        req = req.set("X-Aliyun-Captcha-Verify-Region", r.trim());
    }
    let resp = req
        .send_json(serde_json::json!({ "plan_id": plan_id }))
        .map_err(|e| ClaimError {
            code: -1,
            message: http_err(&crate::i18n::tr("err.claim.claim_req"), e),
            next_at: None,
        })?
        .into_string()
        .map_err(|e| ClaimError {
            code: -1,
            message: crate::i18n::trf("err.http.read", &[("e", &e.to_string())]),
            next_at: None,
        })?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(claim_error(code, &v));
    }
    Ok(v)
}

pub fn fetch_captcha_config() -> Result<CaptchaConfig, String> {
    let mut req = agent().get(CLIENT_CONFIGS_URL);
    for (k, v) in quota::zai_billing_headers("") {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| crate::i18n::trf("err.claim.config_req", &[("e", &e.to_string())]))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    if v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) != 0 {
        return Err(crate::i18n::tr("err.claim.config_unavailable"));
    }
    let c = v.pointer("/data/configs/captcha").cloned().unwrap_or(Value::Null);
    let s = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Ok(CaptchaConfig {
        enabled: c.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
        region: s("region"),
        prefix: s("prefix"),
        scene_id: s("sceneId"),
    })
}

fn str_field<'a>(e: &'a Value, snake: &str, camel: &str) -> Option<&'a str> {
    e.get(snake)
        .and_then(|v| v.as_str())
        .or_else(|| e.get(camel).and_then(|v| v.as_str()))
}

fn num_field(e: &Value, snake: &str, camel: &str) -> Option<f64> {
    e.get(snake)
        .and_then(|v| v.as_f64())
        .or_else(|| e.get(camel).and_then(|v| v.as_f64()))
}

fn parse_plan(p: &Value) -> Option<ClaimPlan> {
    let plan_id = str_field(p, "plan_id", "planId")?.trim().to_string();
    if plan_id.is_empty() {
        return None;
    }
    let grant_items: Vec<ClaimGrant> = p
        .get("entitlements")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    str_field(e, "meter", "meter") == Some("model_usage")
                        && str_field(e, "unit_type", "unitType") == Some("token")
                        && str_field(e, "show_name", "showName")
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                })
                .map(|e| ClaimGrant {
                    name: str_field(e, "show_name", "showName").unwrap_or("").to_string(),
                    units: num_field(e, "grant_units", "grantUnits").unwrap_or(0.0),
                    period: str_field(e, "period", "period").unwrap_or("one_time").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let grants = grant_items
        .iter()
        .map(|g| {
            let period_cn = match g.period.as_str() {
                "daily" => "每日",
                "weekly" => "每周",
                "monthly" => "每月",
                _ => "一次性",
            };
            format!("{} · {} Token（{period_cn}）", g.name, fmt_units(g.units))
        })
        .collect();
    Some(ClaimPlan {
        name: p.get("name").and_then(|s| s.as_str()).unwrap_or("").trim().to_string(),
        description: p
            .get("description")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        priority: p.get("priority").and_then(|x| x.as_i64()).unwrap_or(0),
        plan_id,
        grants,
        grant_items,
    })
}

fn fmt_units(n: f64) -> String {
    let trim = |x: f64| {
        let r = (x * 10.0).round() / 10.0;
        if (r - r.trunc()).abs() < f64::EPSILON {
            format!("{}", r.trunc() as i64)
        } else {
            format!("{r:.1}")
        }
    };
    if n >= 1e8 {
        format!("{}亿", trim(n / 1e8))
    } else if n >= 1e4 {
        format!("{}万", trim(n / 1e4))
    } else {
        format!("{}", n.round() as i64)
    }
}

pub fn failure_message(code: i64, body: &Value) -> String {
    let server_msg = ["msg", "message"]
        .iter()
        .find_map(|k| body.get(k).and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    let base = crate::i18n::tr(match code {
        1001 => "claim.fail.1001",
        1002 => "claim.fail.1002",
        1003 => "claim.fail.1003",
        1004 => "claim.fail.1004",
        1005 => "claim.fail.1005",
        3001 => "claim.fail.3001",
        3007 => "claim.fail.3007",
        401 => "claim.fail.401",
        _ => "claim.fail.generic",
    });
    if server_msg.is_empty() {
        base
    } else {
        crate::i18n::trf("claim.fail.with_server", &[("base", &base), ("server_msg", &server_msg)])
    }
}
