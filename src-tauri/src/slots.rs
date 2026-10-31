//! 多开槽：让多个 ZCode 客户端共存，各自持有一套 subagent 并发槽。
//!
//! 关键点：**不改 ZCode 的并发上限**。每个客户端是独立的 app-server + agent 运行时，
//! 服务端下发的 `limit`（实测 2 个 subagent）在各实例内分别计数，互不仲裁。
//! 这里只做隔离，让多个实例能同时活着：
//!
//!   ZCODE_DESKTOP_APPLICATION_NAME   改 Electron userData → 绕开单实例锁
//!   ZCODE_DESKTOP_USER_DATA_DIR      显式指定 userData（Cookies 独立）
//!   ZCODE_DESKTOP_SESSION_DATA_DIR   会话数据目录
//!
//! 全部是 ZCode 官方支持的环境变量，不重打包、不改安装目录。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::store::{self, Paths};
use crate::zcrypto;

/// z-switch 管理的槽位。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Slot {
    pub id: String,
    pub app_name: String,
    pub user_data_dir: String,
    /// 该槽启动时使用的账号；None 表示没绑定（用户自己开的客户端）
    #[serde(default)]
    pub account_id: Option<String>,
    /// 该槽专用的 device_mid。**必须唯一**：两个实例共用同一个 mid 时，
    /// 服务端会把它们算成同一台设备，共享同一份 subagent 并发额度。
    #[serde(default)]
    pub device_mid: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

/// 给前端的槽视图（合并了运行状态与账号名）。
#[derive(Serialize, Clone, Debug)]
pub struct SlotView {
    pub id: String,
    pub app_name: String,
    pub user_data_dir: String,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub running: bool,
    pub pid: Option<u32>,
    /// true = 不是 z-switch 创建的槽（用户手动开的默认实例）
    pub external: bool,
}

struct RunningMap {
    by_slot: Vec<(String, u32)>,
    external_pids: Vec<u32>,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn slots_file(paths: &Paths) -> PathBuf {
    paths.store_dir().join("slots.json")
}

pub fn load_slots(paths: &Paths) -> Vec<Slot> {
    fs::read_to_string(slots_file(paths))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_slots(paths: &Paths, slots: &[Slot]) -> Result<(), String> {
    let dir = paths.store_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let raw = serde_json::to_string_pretty(slots).map_err(|e| e.to_string())?;
    let target = slots_file(paths);
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &target).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn list_zcode_procs() -> Vec<(u32, String)> {
    if std::env::var("ZCODE_SWITCH_HOME").is_ok() {
        return Vec::new();
    }
    let script = "Get-CimInstance Win32_Process -Filter \"Name='ZCode.exe'\" | ForEach-Object { \"$($_.ProcessId)|$($_.CommandLine)\" }";
    let out = store::no_window("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();
    let Ok(o) = out else { return Vec::new() };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, cmd) = l.split_once('|')?;
            Some((pid.trim().parse::<u32>().ok()?, cmd.to_string()))
        })
        .collect()
}

#[cfg(not(windows))]
fn list_zcode_procs() -> Vec<(u32, String)> {
    Vec::new()
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// 命令行 → 槽 id。只认主进程（带 `--type=` 的是 Electron 子进程）。
fn slot_of_cmd(cmd: &str, slots: &[Slot]) -> Option<String> {
    let lower = cmd.to_lowercase();
    for s in slots {
        let dir = norm(&s.user_data_dir);
        if dir.is_empty() {
            continue;
        }
        if lower.contains(&format!("--user-data-dir={}", dir)) || lower.contains(&dir) {
            return Some(s.id.clone());
        }
    }
    None
}

/// 只认 Electron 主进程：子进程带 `--type=`，而 node 子进程带 `zcode.cjs`。
fn is_main(cmd: &str) -> bool {
    !cmd.contains("--type=") && !cmd.contains("zcode.cjs")
}

fn debug_log(paths: &Paths, msg: &str) {
    use std::io::Write;
    let p = paths.store_dir().join("slots-debug.log");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg);
    }
}

fn running(paths: &Paths) -> RunningMap {
    let slots = load_slots(paths);
    let mut by_slot: Vec<(String, u32)> = Vec::new();
    let mut external_pids: Vec<u32> = Vec::new();
    for (pid, cmd) in list_zcode_procs() {
        if !is_main(&cmd) {
            continue;
        }
        match slot_of_cmd(&cmd, &slots) {
            Some(id) => {
                if !by_slot.iter().any(|(s, _)| s == &id) {
                    by_slot.push((id, pid));
                }
            }
            None => external_pids.push(pid),
        }
    }
    RunningMap {
        by_slot,
        external_pids,
    }
}

pub fn is_running(paths: &Paths, id: &str) -> bool {
    running(paths).by_slot.iter().any(|(s, _)| s == id)
}

/// 读当前 live 的 deviceMid（默认实例启动时用的就是它）。
fn live_mid(paths: &Paths) -> Option<String> {
    let raw = fs::read_to_string(paths.live_telemetry()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("deviceMid")?.as_str().map(|s| s.to_string())
}

/// 挑一个没被任何槽（含默认实例当前 live 值）占用的 mid。
fn fresh_mid(paths: &Paths, slots: &[Slot]) -> String {
    let used: Vec<String> = slots
        .iter()
        .filter_map(|s| s.device_mid.clone())
        .chain(live_mid(paths))
        .collect();
    for _ in 0..16 {
        let candidate = uuid::Uuid::new_v4().to_string();
        if !used.iter().any(|m| m.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    uuid::Uuid::new_v4().to_string()
}

/// 默认实例（「启动 ZCode」/ 直接切换后启动的那个）用的账号要在启动时记下来。
/// 不能运行时读 live —— 之后别的槽会把 live 改写成自己账号的凭据。
pub fn record_default_slot(paths: &Paths) {
    let live_id = store::read_live(paths)
        .ok()
        .flatten()
        .map(|v| zcrypto::account_identity(&v, &paths.home));
    let accounts = store::list_accounts(paths).unwrap_or_default();
    let aid = live_id.as_ref().and_then(|li| {
        if li.user_id.is_none() && li.username.is_none() && li.email.is_none() {
            return None;
        }
        accounts
            .iter()
            .find(|a| {
                let ai = zcrypto::account_identity(&a.credentials, &paths.home);
                (li.user_id.is_some() && li.user_id == ai.user_id)
                    || (li.username.is_some() && li.username == ai.username)
                    || (li.email.is_some() && li.email == ai.email)
            })
            .map(|a| a.id.clone())
    });
    let body = serde_json::json!({ "account_id": aid, "at": now_ts() });
    let _ = fs::create_dir_all(paths.store_dir());
    let _ = fs::write(
        paths.store_dir().join("default-slot.json"),
        body.to_string(),
    );
}

pub fn default_slot_account(paths: &Paths) -> Option<String> {
    let raw = fs::read_to_string(paths.store_dir().join("default-slot.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("account_id")?.as_str().map(|s| s.to_string())
}

/// 默认实例（顶部「启动 ZCode」开出来的那个）也要有固定的 mid，
/// 否则它会和某个槽共用 live 值，两个实例在服务端算成同一台设备。
pub fn ensure_default_mid(paths: &Paths) -> String {
    let f = paths.store_dir().join("default-device-mid.txt");
    if let Ok(s) = fs::read_to_string(&f) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    let slots = load_slots(paths);
    let mid = fresh_mid(paths, &slots);
    let _ = fs::create_dir_all(paths.store_dir());
    let _ = fs::write(&f, &mid);
    mid
}

/// 把槽需要的凭据和 device_mid 写成 live（ZCode 只在启动时读一次）。
fn apply_credentials(paths: &Paths, slot: &Slot) -> Result<(), String> {
    if let Some(aid) = slot.account_id.as_ref() {
        let acc = store::load_account(paths, aid)?;
        store::write_live(paths, &acc.credentials)?;
        if let Some(cfg) = &acc.config {
            let _ = store::write_live_config(paths, cfg);
        }
    }
    // mid 单独处理：不管有没有绑定账号，都要写本槽自己的那一个
    let mid = match slot.device_mid.as_ref().filter(|m| !m.is_empty()) {
        Some(m) => m.clone(),
        None => {
            let mut slots = load_slots(paths);
            let fresh = fresh_mid(paths, &slots);
            if let Some(s) = slots.iter_mut().find(|s| s.id == slot.id) {
                s.device_mid = Some(fresh.clone());
                let _ = save_slots(paths, &slots);
            }
            fresh
        }
    };
    store::write_live_device_mid(paths, &mid)?;
    Ok(())
}

pub fn launch_slot(paths: &Paths, slot: &Slot) -> Result<(), String> {
    let (zcode_path, ok) = store::effective_zcode_path(paths);
    if !ok || zcode_path.trim().is_empty() {
        return Err("请先在设置里指定 ZCode 可执行文件路径".into());
    }
    apply_credentials(paths, slot)?;

    let mut c = Command::new(&zcode_path);
    c.arg(format!("--user-data-dir={}", slot.user_data_dir));
    c.env_remove("ELECTRON_RUN_AS_NODE");
    c.env("ZCODE_DESKTOP_APPLICATION_NAME", &slot.app_name);
    c.env("ZCODE_DESKTOP_USER_DATA_DIR", &slot.user_data_dir);
    c.env(
        "ZCODE_DESKTOP_SESSION_DATA_DIR",
        PathBuf::from(&slot.user_data_dir)
            .join("session")
            .to_string_lossy()
            .to_string(),
    );
    if std::env::var("USERPROFILE").is_ok() {
        let slot_home = PathBuf::from(&slot.user_data_dir).join("home");
        let _ = fs::create_dir_all(&slot_home);
        c.env("ZCODE_DESKTOP_HOME_DIR", slot_home);
    }
    store::detached(c)
        .spawn()
        .map_err(|e| format!("启动失败: {e}"))?;
    Ok(())
}

pub fn kill_slot(paths: &Paths, id: &str) -> Result<bool, String> {
    let slots = load_slots(paths);
    if !slots.iter().any(|s| s.id == id) {
        return Ok(true);
    }
    let pids: Vec<u32> = list_zcode_procs()
        .into_iter()
        .filter(|(_, cmd)| is_main(cmd) && slot_of_cmd(cmd, &slots).as_deref() == Some(id))
        .map(|(p, _)| p)
        .collect();
    for pid in &pids {
        let _ = store::no_window("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let alive = list_zcode_procs()
            .into_iter()
            .any(|(_, cmd)| is_main(&cmd) && slot_of_cmd(&cmd, &slots).as_deref() == Some(id));
        if !alive {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(false)
}

/// 新建一个槽。`account_id` 有值时该槽启动即登录这个账号。
pub fn create_slot(paths: &Paths, account_id: Option<String>) -> Result<Slot, String> {
    let mut slots = load_slots(paths);
    let mut n = 1usize;
    while slots.iter().any(|s| s.id == n.to_string()) {
        n += 1;
    }
    if n > 9 {
        return Err("槽位已满（最多 9 个）".into());
    }
    let dir = paths.store_dir().join("slots").join(format!("slot{n}"));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // 绑定账号时沿用该账号的虚拟 mid；没绑定就新生成一个，避免与默认实例撞车
    let device_mid = match &account_id {
        Some(aid) => store::ensure_virtual_device_mid(paths, aid).ok(),
        None => Some(fresh_mid(paths, &slots)),
    };
    let slot = Slot {
        id: n.to_string(),
        app_name: format!("ZCodeSlot{n}"),
        user_data_dir: dir.to_string_lossy().to_string(),
        account_id,
        device_mid,
        created_at: now_ts(),
    };
    slots.push(slot.clone());
    save_slots(paths, &slots)?;
    Ok(slot)
}

/// 把账号指派到某个槽：写凭据 → 若在运行则重启该槽。
pub fn assign(paths: &Paths, id: &str, account_id: &str) -> Result<Slot, String> {
    let mut slots = load_slots(paths);
    let idx = slots
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| "槽不存在".to_string())?;
    // 绑定账号时，mid 跟着账号走；取不到就保留原值，最后兜底生成一个
    let mid = store::ensure_virtual_device_mid(paths, account_id)
        .ok()
        .or_else(|| slots[idx].device_mid.clone())
        .or_else(|| Some(fresh_mid(paths, &slots)));
    slots[idx].account_id = Some(account_id.to_string());
    slots[idx].device_mid = mid;
    save_slots(paths, &slots)?;
    let slot = slots[idx].clone();
    if is_running(paths, id) {
        kill_slot(paths, id)?;
    }
    launch_slot(paths, &slot)?;
    Ok(slot)
}

/// 移除槽记录（默认实例不能移除）。
pub fn remove_slot(paths: &Paths, id: &str) -> Result<(), String> {
    if is_running(paths, id) {
        kill_slot(paths, id)?;
    }
    let mut slots = load_slots(paths);
    slots.retain(|s| s.id != id);
    save_slots(paths, &slots)
}

pub fn views(paths: &Paths) -> Vec<SlotView> {
    let slots = load_slots(paths);
    let accounts = store::list_accounts(paths).unwrap_or_default();
    let raw = list_zcode_procs();
    let mains: Vec<&String> = raw
        .iter()
        .filter(|(_, c)| is_main(c))
        .map(|(_, c)| c)
        .collect();
    // 用身份比对而不是整体 hash：ZCode 运行时会往 live 里加字段，hash 会漂移
    let live_id = store::read_live(paths)
        .ok()
        .flatten()
        .map(|v| zcrypto::account_identity(&v, &paths.home));
    let live_name = live_id.as_ref().and_then(|li| {
        if li.user_id.is_none() && li.username.is_none() && li.email.is_none() {
            return None;
        }
        accounts
            .iter()
            .find(|a| {
                let ai = zcrypto::account_identity(&a.credentials, &paths.home);
                (li.user_id.is_some() && li.user_id == ai.user_id)
                    || (li.username.is_some() && li.username == ai.username)
                    || (li.email.is_some() && li.email == ai.email)
            })
            .map(|a| a.name.clone())
    });
    debug_log(
        paths,
        &format!(
            "views: procs={} mains={} slots={} accounts={} live={:?}",
            raw.len(),
            mains.len(),
            slots.len(),
            accounts.len(),
            live_name
        ),
    );
    let name_of = |aid: &str| {
        accounts
            .iter()
            .find(|a| a.id == aid)
            .map(|a| a.name.clone())
    };
    let run = running(paths);

    let mut out: Vec<SlotView> = slots
        .iter()
        .map(|s| {
            let pid = run
                .by_slot
                .iter()
                .find(|(sid, _)| sid == &s.id)
                .map(|(_, p)| *p);
            SlotView {
                id: s.id.clone(),
                app_name: s.app_name.clone(),
                user_data_dir: s.user_data_dir.clone(),
                account_id: s.account_id.clone(),
                account_name: s.account_id.as_ref().and_then(|a| name_of(a)),
                running: pid.is_some(),
                pid,
                external: false,
            }
        })
        .collect();

    if !run.external_pids.is_empty() {
        // 优先用「启动默认实例时记下的账号」，live 可能已被别的槽改写
        let recorded = default_slot_account(paths);
        let active = recorded
            .as_ref()
            .and_then(|aid| accounts.iter().find(|a| &a.id == aid))
            .or_else(|| {
                live_id.as_ref().and_then(|li| {
                    if li.user_id.is_none() && li.username.is_none() && li.email.is_none() {
                        return None;
                    }
                    accounts.iter().find(|a| {
                        let ai = zcrypto::account_identity(&a.credentials, &paths.home);
                        (li.user_id.is_some() && li.user_id == ai.user_id)
                            || (li.username.is_some() && li.username == ai.username)
                            || (li.email.is_some() && li.email == ai.email)
                    })
                })
            });
        out.push(SlotView {
            id: "external".into(),
            app_name: "ZCode".into(),
            user_data_dir: String::new(),
            account_id: active.map(|a| a.id.clone()),
            account_name: active.map(|a| a.name.clone()),
            running: true,
            pid: run.external_pids.first().copied(),
            external: true,
        });
    }
    out
}
