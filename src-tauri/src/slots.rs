//! 多开槽：让多个 ZCode 客户端共存，各自持有一套 subagent 并发槽。
//!
//! 关键结论（实测）：服务端的并发额度**按账号记账**（每账号约 3 个 subagent，
//! 超额返回 `rate_limited / "user concurrency limit exceeded"`）。所以多开的价值是
//! **让多个不同账号同时跑**，而不是让同一个号突破上限。
//!
//! 隔离手段全部是 ZCode 官方支持的环境变量，不重打包、不改安装目录：
//!   ZCODE_DESKTOP_APPLICATION_NAME   改 Electron userData → 绕开单实例锁
//!   ZCODE_DESKTOP_USER_DATA_DIR      显式指定 userData（Cookies 独立）
//!   ZCODE_DESKTOP_SESSION_DATA_DIR   会话数据目录
//!   ZCODE_DATA_BASE_DIR              **把整个 `.zcode` 数据根搬进槽目录**（键）
//!   ZCODE_TELEMETRY_DEVICE_MID       直接指定设备 mid（双保险）
//!   --user-data-dir=<槽目录>          同时让「按槽关进程」可定位
//!
//! `ZCODE_DATA_BASE_DIR` 由 bundle 的 `getDataBaseDir()` 读取：
//!   dataRoot = `$ZCODE_DATA_BASE_DIR/.zcode`，凭据 / telemetry-state / 会话库 / 日志
//!   全在这棵树下。设了它，每个客户端就各有一份 credentials 与 deviceMid，
//!   不再依赖「写全局文件 → 立刻 spawn」的抢读时序（那套在实测中会被下一个
//!   客户端覆盖，导致两个客户端共用同一台「设备」）。
//!
//! 进程归属**用 PID 直管**，不去猜命令行：
//!   spawn() 直接拿到 PID 存进 slots.json，存活判定 = 进程快照里有该 PID，
//!   且进程创建时间与记账时间对得上（防 Windows 复用 PID 后误判/误杀）。
//!
//! 读写纪律：`read_views()` 是纯读、绝不落盘；所有落盘都经过 `save_slots()`，
//! 而它要求调用方已持有 `STORE_LOCK`（见 lib.rs 的 `store_guard()`）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::flowlog;
use crate::store::{self, Paths};
use crate::zcrypto;

/// 槽位上限。真正的约束是内存（每个客户端约 1 GB），不是这个数字。
const MAX_SLOTS: usize = 32;
const EXIT_TIMEOUT: Duration = Duration::from_secs(8);
const EXIT_POLL: Duration = Duration::from_millis(250);
/// 启动后的「落地」等待上限：等新进程真正开始启动，才允许下一次启动写 live 文件。
const SETTLE_TIMEOUT: Duration = Duration::from_secs(6);
const SETTLE_POLL: Duration = Duration::from_millis(100);
/// slots.json 的落盘版本。读取端同时兼容旧的无版本数组格式。
const SLOT_FILE_VERSION: u32 = 2;

/// 启动闸门：所有会写 live 文件（凭据 / device_mid）的启动动作都串行。
/// ZCode 是启动过程中才读这些文件，两个启动重叠就会互相覆盖（实测风险）。
static LAUNCH_GATE: Mutex<()> = Mutex::new(());

pub fn launch_guard() -> std::sync::MutexGuard<'static, ()> {
    match LAUNCH_GATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_ms() -> i64 {
    UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Slot {
    pub id: String,
    pub app_name: String,
    pub user_data_dir: String,
    /// 该槽启动时使用的账号；None 表示没绑定
    #[serde(default)]
    pub account_id: Option<String>,
    /// 该槽专用的 device_mid。两个实例共用同一个 mid 时，服务端会把它们算成
    /// 同一台设备、共享同一份 subagent 额度（实测踩坑）。
    #[serde(default)]
    pub device_mid: Option<String>,
    /// 启动时拿到的 PID；0 = 从未启动或已清理
    #[serde(default)]
    pub pid: u32,
    /// 该 PID 对应进程的创建时间（epoch ms）；0 = 旧记录或没取到（降级只看 PID）
    #[serde(default)]
    pub pid_started_at: i64,
    /// userData 目录轮换代数。复用槽时 +1，保证新账号不继承旧账号的 Cookies。
    #[serde(default)]
    pub data_epoch: u32,
    #[serde(default)]
    pub created_at: i64,
    /// 最后一次被启动/指派的时间，供 GC 判断「闲置多久了」
    #[serde(default)]
    pub last_used_at: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct SlotView {
    pub id: String,
    pub app_name: String,
    pub user_data_dir: String,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub running: bool,
    pub pid: u32,
    /// true = 不是 z-switch 创建的槽（用户自己开的默认实例）
    pub external: bool,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Slot {
    /// 这个槽的客户端是否还活着。
    ///
    /// 两道判据：记录的 PID 对得上（含创建时间容差）；或者**命令行里带着同一个
    /// `--user-data-dir`** 的进程还在。后者是必须的 —— ZCode 首次启动/更新后会
    /// 自己重新拉起主进程，PID 变了但数据目录没变，只认 PID 会把活着的槽判成死的，
    /// 进而允许同一账号再开一个窗口（两个窗口同一个数据根 → 额度又串起来）。
    pub fn running(&self, snap: &store::ProcSnapshot) -> bool {
        snap.alive(self.pid, self.pid_started_at) || snap.by_dir(&self.user_data_dir).is_some()
    }

    /// 当前实际应该记的 PID：优先用按目录找回来的（客户端自重启后是新 PID）。
    pub fn live_pid(&self, snap: &store::ProcSnapshot) -> Option<(u32, i64)> {
        if self.pid != 0 && snap.alive(self.pid, self.pid_started_at) {
            return Some((self.pid, self.pid_started_at));
        }
        snap.by_dir(&self.user_data_dir)
    }
}

pub fn slots_file(paths: &Paths) -> PathBuf {
    paths.store_dir().join("slots.json")
}

/// 读取兼容两种格式：v2 对象（`{ version, slots }`）与旧的无版本数组。
#[derive(Deserialize)]
#[serde(untagged)]
enum SlotsRaw {
    Object {
        slots: Vec<Slot>,
        #[serde(default)]
        version: u32,
    },
    Legacy(Vec<Slot>),
}

#[derive(Serialize)]
struct SlotsFileOut<'a> {
    version: u32,
    slots: &'a [Slot],
}

pub fn load_slots(paths: &Paths) -> Vec<Slot> {
    let f = slots_file(paths);
    let Ok(raw) = fs::read_to_string(&f) else {
        return Vec::new(); // 文件不存在是正常状态（首次运行）
    };
    if raw.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<SlotsRaw>(&raw) {
        Ok(SlotsRaw::Object { slots, version }) => {
            if version != SLOT_FILE_VERSION {
                flowlog::log(
                    "slots",
                    "load-legacy-version",
                    &format!("version={version} n={}", slots.len()),
                );
            }
            slots
        }
        Ok(SlotsRaw::Legacy(v)) => {
            flowlog::log("slots", "load-legacy-array", &format!("n={}", v.len()));
            v
        }
        Err(e) => {
            // 解析失败：先把原始内容留一份，再返回空表。绝不删除原文件。
            backup_bad(paths, &raw);
            flowlog::log("slots", "parse-fail", &e.to_string());
            Vec::new()
        }
    }
}

fn backup_bad(paths: &Paths, raw: &str) {
    let dir = paths.store_dir();
    let _ = fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f");
    let p = dir.join(format!("slots.json.bad-{stamp}"));
    match fs::write(&p, raw) {
        Ok(()) => flowlog::log("slots", "backup-bad", &p.to_string_lossy()),
        Err(e) => eprintln!("[zsw] slots.json 备份失败: {e}"),
    }
}

pub fn save_slots(paths: &Paths, slots: &[Slot]) -> Result<(), String> {
    debug_assert!(
        crate::in_store_guard(),
        "save_slots 必须在 store_guard 内调用"
    );
    let dir = paths.store_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = serde_json::to_string_pretty(&SlotsFileOut {
        version: SLOT_FILE_VERSION,
        slots,
    })
    .map_err(|e| e.to_string())?;
    let target = slots_file(paths);
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, body).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &target).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 进程

/// spawn 之后确认进程创建时间。进程刚起来时快照里可能还没刷新到，重试几次。
fn probe_started(pid: u32) -> i64 {
    if pid == 0 || store::in_sandbox() {
        return 0;
    }
    for _ in 0..3 {
        let snap = store::proc_snapshot(true);
        let started = snap.started_of(pid);
        if started != 0 {
            return started;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    0
}

// ---------------------------------------------------------------- mid

fn live_mid(paths: &Paths) -> Option<String> {
    let raw = fs::read_to_string(paths.live_telemetry()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("deviceMid")?.as_str().map(|s| s.to_string())
}

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

/// 默认实例（顶部「启动 ZCode」开出来的那个）也要有固定的 mid，
/// 否则它会和某个槽共用 live 值，两个客户端在服务端算成同一台设备。
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

/// 默认实例用的是哪个账号，要在启动时记下来 —— 之后别的槽会把 live 改写成
/// 自己账号的凭据，运行时再读 live 会把它认错成那个槽的账号（实测踩过）。
pub fn record_default_slot(paths: &Paths) {
    let live_id = store::read_live(paths)
        .ok()
        .flatten()
        .map(|v| zcrypto::account_identity(&v, paths.home.as_path()));
    let accounts = store::list_accounts(paths).unwrap_or_default();
    let aid = live_id.as_ref().and_then(|li| {
        if li.user_id.is_none() && li.username.is_none() && li.email.is_none() {
            return None;
        }
        accounts
            .iter()
            .find(|a| identity_match(li, a, paths))
            .map(|a| a.id.clone())
    });
    let body = serde_json::json!({ "account_id": aid, "at": now_ts() });
    let _ = fs::create_dir_all(paths.store_dir());
    let _ = fs::write(paths.store_dir().join("default-slot.json"), body.to_string());
    flowlog::log(
        "slots",
        "default-slot",
        &format!("account={}", aid.as_deref().unwrap_or("-")),
    );
}

fn default_slot_account(paths: &Paths) -> Option<String> {
    let raw = fs::read_to_string(paths.store_dir().join("default-slot.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("account_id")?.as_str().map(|s| s.to_string())
}

// ---------------------------------------------------------------- mid 归属

/// 默认实例（命令行里没有 `--user-data-dir` 的 ZCode 主进程）是否在跑。
/// 它在跑的时候，**任何改写 live 根（凭据 / deviceMid）的动作都必须放弃**：
/// 那等于把另一个账号的身份塞给一个活着的客户端，两个客户端会被服务端算成同一台设备。
fn default_client_pid(snap: &store::ProcSnapshot) -> Option<u32> {
    snap.mains
        .iter()
        .copied()
        .find(|pid| snap.pids.iter().any(|p| p.pid == *pid && p.data_dir.is_empty()))
}

/// 给槽挑 mid：**避开当前已在跑的客户端（其他槽 + 默认实例）正在用的 mid**。
/// 实测（2026-11-14，另一台机器）：默认实例在跑时给 neurs757 开槽，
/// 槽又把 live 根的 deviceMid 写成了自己的，结果两个客户端上报同一个 mid，
/// 服务端按「一台设备」计数，两边合计仍只有 4 个 subagent。
/// `salt` 传入正在准备的槽 id 时，它自己的旧 mid 不参与冲突判定（允许原地复用）。
fn pick_slot_mid(
    paths: &Paths,
    account_id: Option<&str>,
    snap: &store::ProcSnapshot,
    salt: Option<&str>,
) -> String {
    let slots = load_slots(paths);
    let busy: Vec<String> = slots
        .iter()
        .filter(|s| s.running(snap))
        .filter(|s| salt.map_or(true, |x| s.id != x))
        .filter_map(|s| s.device_mid.clone())
        .chain(live_mid(paths))
        .collect();
    let canonical = account_id.and_then(|aid| store::ensure_virtual_device_mid(paths, aid).ok());
    match canonical {
        Some(m) if !m.is_empty() && !busy.iter().any(|b| b.eq_ignore_ascii_case(&m)) => m,
        _ => fresh_mid(paths, &slots),
    }
}


fn identity_match(li: &zcrypto::Identity, acc: &store::Account, paths: &Paths) -> bool {
    let ai = zcrypto::account_identity(&acc.credentials, paths.home.as_path());
    (li.user_id.is_some() && li.user_id == ai.user_id)
        || (li.username.is_some() && li.username == ai.username)
        || (li.email.is_some() && li.email == ai.email)
}

// ---------------------------------------------------------------- 启动 / 关闭

/// 槽自己的 ZCode 数据根。设了 `ZCODE_DATA_BASE_DIR=<user_data_dir>`，
/// bundle 的 `getDataBaseDir()` 就会把 `.zcode` 挂到它下面。
fn slot_zcode_root(slot: &Slot) -> PathBuf {
    PathBuf::from(&slot.user_data_dir).join(".zcode")
}

/// 把凭据 / device_mid / 必要配置播种到槽自己的数据根。
///
/// 这是额度解耦的核心一步：每个客户端从此各有一份 `credentials.json` 与
/// `telemetry-state.json`，不再靠「写全局文件 → 立刻 spawn」的抢读时序。
/// 只补缺不覆盖（配置类文件）；凭据与 mid 每次启动都对齐当前绑定。
fn seed_slot_data_root(
    paths: &Paths,
    slot: &Slot,
    creds: Option<&Value>,
    mid: &str,
) -> Result<(), String> {
    let v2 = slot_zcode_root(slot).join("v2");
    fs::create_dir_all(&v2).map_err(|e| e.to_string())?;

    if let Some(c) = creds {
        let body = serde_json::to_string_pretty(c).unwrap_or_default() + "\n";
        store::atomic_write(&v2.join("credentials.json"), &body)?;
    }

    // 这些是「客户端能正常起来」需要的静态配置，槽里没有就从全局拷一份。
    for name in [
        "provider_config.json",
        "setting.json",
        "onboarding-record.json",
        "bot-config.v3.json",
        "bot-state.v3.json",
    ] {
        let dst = v2.join(name);
        if dst.exists() {
            continue;
        }
        let src = paths.zcode_dir().join("v2").join(name);
        if src.exists() {
            let _ = fs::copy(&src, &dst);
        }
    }

    let tele = v2.join("telemetry-state.json");
    let mut v: Value = fs::read_to_string(&tele)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|x| x.is_object())
        .unwrap_or_else(|| json!({}));
    if v.get("deviceMid").and_then(|m| m.as_str()) != Some(mid) {
        v["deviceMid"] = json!(mid);
        store::atomic_write(&tele, &(serde_json::to_string(&v).unwrap_or_default() + "\n"))?;
    }
    Ok(())
}

/// 槽的凭据与 mid：只播种到槽**自己的**数据根。
///
/// 2026-11-14 起不再改写 live 根的凭据：以前「默认实例没在跑时顺手写 live」会让用户在
/// 某个时间点打开默认客户端时，发现自己登的已经是槽里那个账号 —— ZCode 的会话列表
/// 跟着当前登录账号走，用户就会以为「几个工程的会话全丢了」（数据其实还在）。
/// 换默认实例的账号只能由用户显式的「切换」动作完成。
fn apply_credentials(paths: &Paths, slot: &mut Slot) -> Result<(), String> {
    let mut creds: Option<Value> = None;
    if let Some(aid) = slot.account_id.clone() {
        let acc = store::load_account(paths, &aid)?;
        flowlog::log(
            "slots",
            "slot-no-live-write",
            &format!("slot={} acct={aid} (live 根凭据不动)", slot.id),
        );
        creds = Some(acc.credentials.clone());
    }
    let mid = match slot.device_mid.clone().filter(|m| !m.is_empty()) {
        Some(m) => m,
        None => {
            let slots = load_slots(paths);
            let fresh = fresh_mid(paths, &slots);
            slot.device_mid = Some(fresh.clone());
            fresh
        }
    };
    // 槽的 deviceMid **绝不写进 live 根**：live 根是默认实例的地盘，
    // 写进去会让之后手动启动的默认客户端顶着槽的 mid 上线（两个客户端撞成同一台设备，
    // 实测 2026-11-14 另一台机器：real/slot 都是 neurs757 的 mid，合计只剩 4 个 subagent）。
    // 槽自己的 mid 已经通过 ZCODE_DATA_BASE_DIR 播种 + ZCODE_TELEMETRY_DEVICE_MID 注入。
    seed_slot_data_root(paths, slot, creds.as_ref(), &mid)?;
    Ok(())
}

/// 修掉 live 根里的历史污染：live 的 deviceMid 若与某个槽相同，且默认实例当前没在跑，
/// 就把它改回默认实例自己的 mid。默认实例在跑时不动（那是正在使用的身份，改了更糟）。
pub fn heal_live_mid_collision(paths: &Paths) -> Option<(String, String)> {
    let snap = store::proc_snapshot(true);
    if default_client_pid(&snap).is_some() {
        return None;
    }
    let live = live_mid(paths)?;
    let slots = load_slots(paths);
    if !slots.iter().any(|s| {
        s.device_mid
            .as_deref()
            .map_or(false, |m| m.eq_ignore_ascii_case(&live))
    }) {
        return None;
    }
    let mid = ensure_default_mid(paths);
    if mid.eq_ignore_ascii_case(&live) {
        return None;
    }
    let _ = store::write_live_device_mid(paths, &mid);
    flowlog::log(
        "slots",
        "heal-live-mid",
        &format!("old={live} new={mid}"),
    );
    Some((live, mid))
}

/// 启动后的落地判定：等新进程在 userData 下写出启动痕迹，再放闸门。
/// 有痕迹（Electron 已完成 profile 初始化）通常意味着它已经读过 live 文件；
/// 等不到就用超时兜底，绝不无限等。
fn settle_after_spawn(slot: &Slot, spawn_at_ms: i64) -> (&'static str, u128) {
    if store::in_sandbox() {
        return ("sandbox", 0);
    }
    let started = Instant::now();
    let ud = PathBuf::from(&slot.user_data_dir);
    let deadline = started + SETTLE_TIMEOUT;
    loop {
        for name in ["Preferences", "Local State"] {
            let p = ud.join(name);
            if let Ok(md) = fs::metadata(&p) {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if mtime >= spawn_at_ms {
                    return ("userdata", started.elapsed().as_millis());
                }
            }
        }
        if Instant::now() >= deadline {
            return ("timeout", started.elapsed().as_millis());
        }
        std::thread::sleep(SETTLE_POLL);
    }
}

/// 启动一个槽。已在运行则直接返回原 PID，不重复开。
pub fn launch_slot(paths: &Paths, slot: &mut Slot) -> Result<u32, String> {
    let snap = store::proc_snapshot(true);
    if let Some((pid, started)) = slot.live_pid(&snap) {
        // 已经在跑（可能 PID 变了）：把记录对齐后直接返回，绝不再开第二个窗口。
        if slot.pid != pid || slot.pid_started_at != started {
            slot.pid = pid;
            slot.pid_started_at = started;
            flowlog::log(
                "slots",
                "launch-skip",
                &format!("slot={} pid={pid} already running", slot.id),
            );
        }
        return Ok(pid);
    }
    let (zcode_path, ok) = store::effective_zcode_path(paths);
    if !ok || zcode_path.trim().is_empty() {
        return Err("请先在设置里指定 ZCode 可执行文件路径".into());
    }
    // 从这里到 settle 结束，任何别的启动都不能插进来改写 live 文件。
    let _gate = launch_guard();
    // mid 撞车就地纠正：与 live 根或其他在跑客户端的 mid 相同 → 换一个新的。
    // 服务端按 device mid 认定「设备」，撞了就共享额度（实测 2026-11-14）。
    let mut mid_fixed = false;
    if let Some(cur) = slot.device_mid.clone().filter(|m| !m.is_empty()) {
        let live = live_mid(paths);
        let others = load_slots(paths);
        let clash_live = live
            .as_deref()
            .map(|l| l.eq_ignore_ascii_case(&cur))
            .unwrap_or(false)
            && default_client_pid(&snap).is_some();
        let clash_slot = others.iter().any(|s| {
            s.id != slot.id
                && s.running(&snap)
                && s.device_mid
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case(&cur))
                    .unwrap_or(false)
        });
        if clash_live || clash_slot {
            let fresh = fresh_mid(paths, &others);
            flowlog::log(
                "slots",
                "mid-collision-fixed",
                &format!(
                    "slot={} old={} new={} live={}",
                    slot.id,
                    cur,
                    fresh,
                    clash_live
                ),
            );
            slot.device_mid = Some(fresh);
            mid_fixed = true;
        }
    }
    apply_credentials(paths, slot)?;
    let spawn_at = now_ms();

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
        let slot_home = PathBuf::from(&slot.user_data_dir);
        let _ = fs::create_dir_all(&slot_home);
        // 桌面侧配置目录与数据根同源，避免 setting.json 又落回全局。
        c.env("ZCODE_DESKTOP_HOME_DIR", &slot_home);
        // 关键隔离：整个 .zcode 数据根（凭据 / telemetry / 会话库 / 日志）搬进槽目录。
        c.env("ZCODE_DATA_BASE_DIR", &slot_home);
        // 双保险：直接指定设备 mid，不依赖文件读取时机。
        if let Some(mid) = slot.device_mid.clone().filter(|m| !m.is_empty()) {
            c.env("ZCODE_TELEMETRY_DEVICE_MID", mid);
        }
    }

    let child = store::detached(c)
        .spawn()
        .map_err(|e| format!("启动失败: {e}"))?;
    let pid = child.id();
    drop(child); // 进程独立运行，句柄不用留

    // 等子进程真正进入启动流程再放闸门：只有它已经起来，后续启动改写 live 才安全。
    let (level, waited) = settle_after_spawn(slot, spawn_at);
    let started_at = probe_started(pid);
    let mut slots = load_slots(paths);
    if let Some(s) = slots.iter_mut().find(|s| s.id == slot.id) {
        s.pid = pid;
        s.pid_started_at = started_at;
        s.last_used_at = now_ts();
        s.device_mid = slot.device_mid.clone();
        s.account_id = slot.account_id.clone();
        save_slots(paths, &slots)?;
    }
    slot.pid = pid;
    slot.pid_started_at = started_at;
    slot.last_used_at = now_ts();
    let isolated = slot_zcode_root(slot).join("v2").is_dir();
    flowlog::log(
        "slots",
        "launch",
        &format!(
            "slot={} pid={} started={} settle={} wait={}ms acct={} mid={} liveMid={} liveRunning={} midFixed={} isolated={}",
            slot.id,
            pid,
            started_at,
            level,
            waited,
            slot.account_id.as_deref().unwrap_or("-"),
            slot.device_mid.as_deref().unwrap_or("-"),
            live_mid(paths).unwrap_or_else(|| "-".to_string()),
            default_client_pid(&snap).is_some(),
            mid_fixed,
            isolated
        ),
    );
    Ok(pid)
}

/// 关闭单个槽，不碰其他客户端。
///
/// 目标进程 = 记录里的 PID（若还对得上）+ 命令行里带同一个 `--user-data-dir` 的
/// 全部进程。后者兜住「客户端自重启后 PID 变了」的情况，否则旧窗口关不掉。
/// 只重启**一个**槽：先温柔关窗（不发 /F，让客户端自己把会话写盘），
/// 关不掉再强杀这一棵进程树（`/T` 只含它的子进程，碰不到别的实例和默认客户端），
/// 最后用**同一个槽、同一个数据根**重新拉起 —— 历史会话在原数据根里，账号不会被换掉。
///
/// 用于「某个客户端白屏但别的窗口还在跑」：白屏是 ZCode 渲染进程崩了（GPU 子进程先崩），
/// ZCode 自己也会重开，但可能等十几秒到几分钟；这里让用户可以立刻把它单独重启。
/// 代价：该窗口**正在生成**的那一轮会中断（已落盘的消息不会丢）。
pub fn restart_slot(paths: &Paths, id: &str) -> Result<u32, String> {
    let mut slots = load_slots(paths);
    let Some(idx) = slots.iter().position(|s| s.id == id) else {
        return Err(format!("槽 {id} 不存在"));
    };
    let dir = slots[idx].user_data_dir.clone();
    let snap = store::proc_snapshot(true);
    let mut targets: Vec<u32> = Vec::new();
    if let Some((pid, _)) = slots[idx].live_pid(&snap) {
        if pid != 0 {
            targets.push(pid);
        }
    }
    for pid in snap.pids_by_dir(&dir) {
        if !targets.contains(&pid) {
            targets.push(pid);
        }
    }

    if !targets.is_empty() {
        flowlog::log("slots", "restart-kill", &format!("slot={id} pids={targets:?}"));
        // 第一步：只发 WM_CLOSE（不带 /F），GUI 客户端收到关闭请求会先 flush
        #[cfg(windows)]
        for pid in &targets {
            let _ = store::no_window("taskkill")
                .args(["/PID", &pid.to_string()])
                .output();
        }
        #[cfg(not(windows))]
        for pid in &targets {
            let _ = store::no_window("kill")
                .args(["-TERM", &pid.to_string()])
                .output();
        }
        let soft = Instant::now() + Duration::from_secs(3);
        while Instant::now() < soft {
            if store::proc_snapshot(false).by_dir(&dir).is_none() {
                break;
            }
            std::thread::sleep(EXIT_POLL);
        }
        // 第二步：赖着不走就强杀这棵树（/T 只带它的子进程）
        if store::proc_snapshot(true).by_dir(&dir).is_some() {
            flowlog::log("slots", "restart-force", &format!("slot={id} pids={targets:?}"));
            #[cfg(windows)]
            for pid in &targets {
                let _ = store::no_window("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
            #[cfg(not(windows))]
            for pid in &targets {
                let _ = store::no_window("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
            let hard = Instant::now() + EXIT_TIMEOUT;
            while Instant::now() < hard {
                if store::proc_snapshot(false).by_dir(&dir).is_none() {
                    break;
                }
                std::thread::sleep(EXIT_POLL);
            }
        }
        if store::proc_snapshot(true).by_dir(&dir).is_some() {
            return Err("该客户端没能在超时内退出，重启已取消（没有重新拉起）".into());
        }
    }

    slots[idx].pid = 0;
    slots[idx].pid_started_at = 0;
    save_slots(paths, &slots)?;
    let mut slot = slots[idx].clone();
    let new_pid = launch_slot(paths, &mut slot)?;
    flowlog::log(
        "slots",
        "restart",
        &format!("slot={id} old={targets:?} new={new_pid}"),
    );
    Ok(new_pid)
}

pub fn kill_slot(paths: &Paths, id: &str) -> Result<bool, String> {
    let mut slots = load_slots(paths);
    let Some(idx) = slots.iter().position(|s| s.id == id) else {
        return Ok(true);
    };
    let snap = store::proc_snapshot(true);
    let dir = slots[idx].user_data_dir.clone();
    let mut targets: Vec<u32> = Vec::new();
    if let Some((pid, _)) = slots[idx].live_pid(&snap) {
        if pid != 0 {
            targets.push(pid);
        }
    }
    for pid in snap.pids_by_dir(&dir) {
        if !targets.contains(&pid) {
            targets.push(pid);
        }
    }
    let mut still = false;
    if !targets.is_empty() {
        flowlog::log("slots", "kill", &format!("slot={id} pids={targets:?}"));
        for pid in &targets {
            #[cfg(windows)]
            {
                let _ = store::no_window("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
            #[cfg(not(windows))]
            {
                let _ = store::no_window("kill").args(["-9", &pid.to_string()]).output();
            }
        }
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while Instant::now() < deadline {
            let now = store::proc_snapshot(false);
            let any_alive = targets.iter().any(|p| {
                now.pids.iter().any(|i| i.pid == *p)
            });
            if !any_alive && now.by_dir(&dir).is_none() {
                break;
            }
            std::thread::sleep(EXIT_POLL);
        }
        let now = store::proc_snapshot(true);
        still = now.by_dir(&dir).is_some()
            || targets.iter().any(|p| now.pids.iter().any(|i| i.pid == *p));
    }
    slots[idx].pid = 0;
    slots[idx].pid_started_at = 0;
    save_slots(paths, &slots)?;
    if still {
        flowlog::log("slots", "kill-timeout", &format!("slot={id} pids={targets:?}"));
    }
    Ok(!still)
}

// ---------------------------------------------------------------- 槽管理

/// 每个槽（每一代）独立的 userData 目录。复用槽时换新目录，
/// 旧账号的 Cookies / LocalStorage 就不会被新账号继承。
fn slot_data_dir(paths: &Paths, id: &str, epoch: u32) -> PathBuf {
    let tag = uuid::Uuid::new_v4().to_string();
    let short = &tag[..8];
    paths
        .store_dir()
        .join("slots")
        .join(format!("slot{id}-e{epoch}-{short}"))
}

/// 新建槽。回收规则：**任何没在跑的槽都可以复用**（不限是否绑过账号），
/// 且取编号最小的那个 —— 否则编号只增不减，开十几次就假报「槽位已满」。
pub fn create_slot(paths: &Paths, account_id: Option<String>) -> Result<Slot, String> {
    let mut slots = load_slots(paths);
    let snap = store::proc_snapshot(true);

    let reusable = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.running(&snap))
        .min_by_key(|(_, s)| s.id.parse::<u32>().unwrap_or(u32::MAX))
        .map(|(i, _)| i);
    if let Some(i) = reusable {
        let id = slots[i].id.clone();
        // **数据根跟账号走，不跟槽号走**：同一个账号永远落在同一个目录里。
        // 以前按「槽号 + epoch」换目录，等于同一个号每多开一次就换一个全新的空根，
        // 历史（tasks-index / cli\db / workspace）明明还在磁盘上，窗口里却全空 —
        // 用户会以为「多开把会话弄丢了」（2026-11-14 实测反馈）。
        let dir = match account_id.as_deref() {
            Some(aid) => dir_for_account(paths, &slots, aid),
            None => keep_dir_or_epoch(paths, &slots, i),
        };
        let is_new = !dir.is_dir();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mid = pick_slot_mid(paths, account_id.as_deref(), &snap, None);
        slots[i].account_id = account_id;
        slots[i].device_mid = Some(mid);
        slots[i].pid = 0;
        slots[i].pid_started_at = 0;
        slots[i].user_data_dir = dir.to_string_lossy().to_string();
        slots[i].app_name = format!("ZCodeSlot{id}");
        slots[i].last_used_at = now_ts();
        save_slots(paths, &slots)?;
        flowlog::log(
            "slots",
            "reuse",
            &format!(
                "slot={id} acct={} dir={} new={is_new}",
                slots[i].account_id.as_deref().unwrap_or("-"),
                slots[i].user_data_dir
            ),
        );
        return Ok(slots[i].clone());
    }

    let mut n = 1usize;
    while slots.iter().any(|s| s.id == n.to_string()) {
        n += 1;
    }
    if n > MAX_SLOTS {
        let running = slots.iter().filter(|s| s.running(&snap)).count();
        return Err(format!(
            "槽位上限 {MAX_SLOTS} 个（当前 {} 条记录，{running} 个在跑），先关掉不用的客户端再开",
            slots.len()
        ));
    }
    let id = n.to_string();
    let dir = match account_id.as_deref() {
        Some(aid) => dir_for_account(paths, &slots, aid),
        None => slot_data_dir(paths, &id, 0),
    };
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let device_mid = Some(pick_slot_mid(paths, account_id.as_deref(), &snap, None));
    let slot = Slot {
        id: id.clone(),
        app_name: format!("ZCodeSlot{n}"),
        user_data_dir: dir.to_string_lossy().to_string(),
        account_id,
        device_mid,
        pid: 0,
        pid_started_at: 0,
        data_epoch: 0,
        created_at: now_ts(),
        last_used_at: now_ts(),
    };
    slots.push(slot.clone());
    save_slots(paths, &slots)?;
    flowlog::log("slots", "create", &format!("slot={} n={}", slot.id, slots.len()));
    Ok(slot)
}

/// 把账号指派到某个槽：写到该账号自己的数据根 → 若在运行则重启该槽。
/// **数据根固定在账号上**（`~/.zcode-switch/profiles/<账号id>`，或它原来就在用的目录），
/// 换账号才换目录：旧账号的 Cookies 留在它自己的目录里，互不串味。
pub fn assign(paths: &Paths, id: &str, account_id: &str) -> Result<Slot, String> {
    let mut slots = load_slots(paths);
    let idx = slots
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| "槽不存在".to_string())?;
    let cur = slots[idx].user_data_dir.clone();
    let dir = if slots[idx].account_id.as_deref() == Some(account_id) && !cur.is_empty() && PathBuf::from(&cur).is_dir() {
        PathBuf::from(&cur)
    } else {
        dir_for_account(paths, &slots, account_id)
    };
    let is_new = !dir.is_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mid = store::ensure_virtual_device_mid(paths, account_id)
        .ok()
        .or_else(|| Some(fresh_mid(paths, &slots)));
    slots[idx].account_id = Some(account_id.to_string());
    slots[idx].device_mid = mid;
    slots[idx].user_data_dir = dir.to_string_lossy().to_string();
    slots[idx].last_used_at = now_ts();
    save_slots(paths, &slots)?;
    flowlog::log(
        "slots",
        "assign",
        &format!("slot={id} acct={account_id} dir={} new={is_new}", slots[idx].user_data_dir),
    );

    if slots[idx].running(&store::proc_snapshot(true)) {
        kill_slot(paths, id)?;
    }
    let mut slot = load_slots(paths)
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| "槽不存在".to_string())?;
    launch_slot(paths, &mut slot)?;
    Ok(slot)
}

/// 清理未运行的槽记录。`older_than_secs <= 0` = 清掉所有未运行的。
/// 只删记录，不删磁盘目录 —— 目录清理是另一个显式动作。
pub fn gc(paths: &Paths, older_than_secs: i64) -> Result<usize, String> {
    let mut slots = load_slots(paths);
    let snap = store::proc_snapshot(true);
    let now = now_ts();
    let before = slots.len();
    slots.retain(|s| {
        if s.running(&snap) {
            return true;
        }
        if older_than_secs <= 0 {
            return false;
        }
        now.saturating_sub(s.last_used_at.max(s.created_at)) < older_than_secs
    });
    let removed = before - slots.len();
    if removed > 0 {
        save_slots(paths, &slots)?;
    }
    flowlog::log(
        "slots",
        "gc",
        &format!("removed={removed} left={}", slots.len()),
    );
    Ok(removed)
}

pub fn remove_slot(paths: &Paths, id: &str) -> Result<(), String> {
    let slots = load_slots(paths);
    let alive = slots
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.running(&store::proc_snapshot(false)))
        .unwrap_or(false);
    if alive {
        kill_slot(paths, id)?;
    }
    let mut slots = load_slots(paths);
    slots.retain(|s| s.id != id);
    save_slots(paths, &slots)?;
    flowlog::log("slots", "remove", &format!("slot={id}"));
    Ok(())
}

/// 「关 ZCode」是全杀，所有槽的 PID 都会失效，顺手清干净，
/// 避免下次读状态时还显示「在跑」。
pub fn clear_all_pids(paths: &Paths) {
    let mut slots = load_slots(paths);
    let mut dirty = false;
    for s in slots.iter_mut() {
        if s.pid != 0 {
            s.pid = 0;
            s.pid_started_at = 0;
            dirty = true;
        }
    }
    if dirty {
        let _ = save_slots(paths, &slots);
    }
}

// ------------------------------------------------------- 数据根取证 / 挂载旧目录

/// 列出磁盘上所有 ZCode 数据根：默认根、当前槽表指向的根、以及槽目录里残留的
/// 旧代目录（换 epoch 后客户端不再指向它，但历史会话还在里面）。
/// 每项带 `tasks-index.sqlite` 的体积/时间 —— 界面里「会话不见了」时，用它找到
/// 真正存放历史的那一份，再用 `adopt_dir` 把窗口指回去。
pub fn root_inventory(paths: &Paths) -> Vec<Value> {
    let snap = store::proc_snapshot(false);
    let slots = load_slots(paths);
    let mut out = Vec::new();
    out.push(root_entry(&paths.zcode_dir(), "default", None, None, &snap));
    for s in slots.iter() {
        let root = PathBuf::from(&s.user_data_dir).join(".zcode");
        out.push(root_entry(&root, "slot", Some(s.id.clone()), s.account_id.clone(), &snap));
    }
    if let Ok(rd) = fs::read_dir(paths.store_dir().join("slots")) {
        for e in rd.flatten() {
            let base = e.path();
            if !base.is_dir() {
                continue;
            }
            let b = base.to_string_lossy().to_string();
            if slots.iter().any(|s| s.user_data_dir.eq_ignore_ascii_case(&b)) {
                continue;
            }
            let root = base.join(".zcode");
            if root.join("v2").is_dir() {
                out.push(root_entry(&root, "old", None, None, &snap));
            }
        }
    }
    out
}

fn root_entry(
    root: &std::path::Path,
    kind: &str,
    slot_id: Option<String>,
    account: Option<String>,
    snap: &store::ProcSnapshot,
) -> Value {
    let base = root
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let idx = root.join("v2").join("tasks-index.sqlite");
    let meta = fs::metadata(&idx).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ws = root.join("workspace");
    let ws_count = fs::read_dir(&ws).map(|r| r.flatten().count()).unwrap_or(0);
    json!({
        "kind": kind,
        "slot": slot_id,
        "account": account,
        "dir": base,
        "root": root.to_string_lossy().to_string(),
        "tasksIndexBytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
        "tasksIndexModified": modified,
        "workspaces": ws_count,
        "running": !snap.pids_by_dir(&base).is_empty(),
    })
}

/// 把「磁盘上已有的某个数据根」挂成一个槽并交给调用方去启动 —— 用于把窗口
/// 指回历史会话所在的那一代目录（多开换代后旧目录原样保留，只是没有槽指向它）。
pub fn adopt_dir(paths: &Paths, dir: &str, account_id: Option<String>) -> Result<Slot, String> {
    let p = PathBuf::from(dir.trim().trim_matches('"'));
    if !p.is_dir() {
        return Err(format!("目录不存在：{}", p.display()));
    }
    let root = p.join(".zcode");
    if !root.join("v2").is_dir() {
        return Err(format!(
            "这不是 ZCode 数据根（{} 下没有 .zcode\\v2）：{}",
            p.display(),
            root.display()
        ));
    }
    let live = paths.zcode_dir();
    if store::normalize_dir(&p.to_string_lossy()) == store::normalize_dir(&live.to_string_lossy()) {
        return Err("这是默认客户端的数据根，请直接启动默认客户端，不要挂成槽".to_string());
    }
    let base = p.to_string_lossy().to_string();
    let snap = store::proc_snapshot(true);
    if !snap.pids_by_dir(&base).is_empty() {
        return Err(format!("这个数据根正被客户端占用，先关掉它：{base}"));
    }
    let mut slots = load_slots(paths);
    if let Some(s) = slots
        .iter()
        .find(|s| s.user_data_dir.eq_ignore_ascii_case(&base))
    {
        flowlog::log("slots", "adopt-existing", &format!("slot={} dir={base}", s.id));
        return Ok(s.clone());
    }
    let mut n = 1usize;
    while slots.iter().any(|s| s.id == n.to_string()) {
        n += 1;
    }
    if n > MAX_SLOTS {
        return Err(format!("槽位上限 {MAX_SLOTS} 个，先关掉不用的客户端再挂"));
    }
    let mid = telemetry_mid(&root)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| pick_slot_mid(paths, account_id.as_deref(), &snap, None));
    let slot = Slot {
        id: n.to_string(),
        app_name: format!("ZCodeSlot{n}"),
        user_data_dir: base.clone(),
        account_id,
        device_mid: Some(mid),
        pid: 0,
        pid_started_at: 0,
        data_epoch: 0,
        created_at: now_ts(),
        last_used_at: now_ts(),
    };
    slots.push(slot.clone());
    save_slots(paths, &slots)?;
    flowlog::log("slots", "adopt", &format!("slot={} dir={base}", slot.id));
    Ok(slot)
}

fn telemetry_mid(root: &std::path::Path) -> Option<String> {
    let f = root.join("v2").join("telemetry-state.json");
    let txt = fs::read_to_string(f).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    v.get("deviceMid")?.as_str().map(|s| s.to_string())
}

/// 每个账号固定一个数据根：`~/.zcode-switch/profiles/<账号id>`。
/// 槽号会被回收、会变，账号不会 —— 把目录挂在账号上，历史就永远跟着这个号。
fn profile_dir(paths: &Paths, account_id: &str) -> PathBuf {
    paths.store_dir().join("profiles").join(account_id)
}

/// 该账号应该用哪个目录：已经在用的（槽表里绑定它且目录存在）优先，其次 profiles 里的固定目录。
fn dir_for_account(paths: &Paths, slots: &[Slot], account_id: &str) -> PathBuf {
    if let Some(s) = slots.iter().find(|s| {
        s.account_id.as_deref() == Some(account_id)
            && !s.user_data_dir.is_empty()
            && PathBuf::from(&s.user_data_dir).is_dir()
    }) {
        return PathBuf::from(&s.user_data_dir);
    }
    profile_dir(paths, account_id)
}

/// 没绑定账号的槽：原目录还在就继续用，否则按 epoch 起一个新目录。
fn keep_dir_or_epoch(paths: &Paths, slots: &[Slot], i: usize) -> PathBuf {
    let cur = PathBuf::from(&slots[i].user_data_dir);
    if cur.is_dir() {
        return cur;
    }
    slot_data_dir(paths, &slots[i].id, slots[i].data_epoch)
}

/// 把一份数据根里的「会话」搬进目标目录：`v2\tasks-index.sqlite`（会话列表索引）+
/// `cli\db\db.sqlite`（消息正文）+ `workspace`（会话草稿目录）。
///
/// 为什么要手动搬：两个客户端各用一个数据根才有独立的账号/设备身份，会话数据也跟着
/// 落在各自的根里 —— 想在新窗口里看到老历史，只能把老根里的会话拷过去。
/// **源目录必须没有客户端在跑**（SQLite 边写边拷会拷出坏库），调用方负责检查。
pub fn copy_history(dst_dir: &Path, src_root: &Path) -> Result<String, String> {
    let dst_root = dst_dir.join(".zcode");
    fs::create_dir_all(dst_root.join("v2")).map_err(|e| e.to_string())?;
    let mut copied: Vec<String> = Vec::new();

    // 1) 会话列表索引
    let idx_src = src_root.join("v2").join("tasks-index.sqlite");
    if !idx_src.exists() {
        return Err(format!("源目录里没有 v2\\tasks-index.sqlite：{}", src_root.display()));
    }
    for name in ["tasks-index.sqlite", "tasks-index.sqlite-wal", "tasks-index.sqlite-shm"] {
        let s = src_root.join("v2").join(name);
        if s.exists() {
            let d = dst_root.join("v2").join(name);
            fs::copy(&s, &d).map_err(|e| format!("拷 {name} 失败：{e}"))?;
            copied.push(name.to_string());
        }
    }

    // 2) 消息正文库（没有就是这个版本的 ZCode 不落本地库，跳过）
    let db_src = src_root.join("cli").join("db").join("db.sqlite");
    if db_src.exists() {
        let db_dst = dst_root.join("cli").join("db");
        fs::create_dir_all(&db_dst).map_err(|e| e.to_string())?;
        for name in ["db.sqlite", "db.sqlite-wal", "db.sqlite-shm"] {
            let s = src_root.join("cli").join("db").join(name);
            if s.exists() {
                let d = db_dst.join(name);
                fs::copy(&s, &d).map_err(|e| format!("拷 cli\\db\\{name} 失败：{e}"))?;
                copied.push(format!("cli/db/{name}"));
            }
        }
    }

    // 3) workspace（会话草稿/工程目录，实测很小；超过 200MB 就跳过，避免把窗口启动拖死）
    let ws_src = src_root.join("workspace");
    if ws_src.is_dir() {
        let bytes = dir_size(&ws_src);
        if bytes <= 200 * 1024 * 1024 {
            let ws_dst = dst_root.join("workspace");
            copy_tree(&ws_src, &ws_dst)?;
            copied.push(format!("workspace ({:.1} MB)", bytes as f64 / 1048576.0));
        } else {
            copied.push(format!("workspace 跳过（{:.0} MB 太大）", bytes as f64 / 1048576.0));
        }
    }
    Ok(copied.join(" + "))
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = fs::read_dir(p) else { return 0 };
    for e in rd.flatten() {
        let path = e.path();
        match e.metadata() {
            Ok(m) if m.is_dir() => total += dir_size(&path),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let rd = fs::read_dir(src).map_err(|e| e.to_string())?;
    for e in rd.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            let _ = fs::copy(&from, &to);
        }
    }
    Ok(())
}

/// 给某个槽灌历史：把源数据根（默认根或某个旧目录）的会话拷进这个槽自己的目录。
/// 槽在跑 / 源目录在里面跑，都直接拒绝 —— 这两个都是「边写边拷」的坏场景。
pub fn seed_history(paths: &Paths, id: &str, from: Option<&str>) -> Result<String, String> {
    let slots = load_slots(paths);
    let slot = slots
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("槽不存在：{id}"))?
        .clone();
    let snap = store::proc_snapshot(true);
    if slot.running(&snap) {
        return Err(format!("槽 {id} 正在运行，先关掉它再灌历史"));
    }
    let src_root = match from {
        Some(d) => PathBuf::from(d.trim().trim_matches('"')).join(".zcode"),
        None => paths.zcode_dir(),
    };
    if !src_root.is_dir() {
        return Err(format!("源数据根不存在：{}", src_root.display()));
    }
    let src_base = src_root
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    // 源目录必须没有客户端在写。默认实例的主进程命令行里没有 --user-data-dir，
    // 靠目录匹配抓不到它 —— 必须单独判一次，否则会「边写边拷」拷出坏库
    // （2026-11-14 实测：默认客户端在跑时先放过了这一关，把 live 的 67MB 消息库
    //  拷进了一个槽，随后手工清掉了这份不该存在的副本）。
    let src_is_live =
        store::normalize_dir(&src_base) == store::normalize_dir(&paths.data_root.to_string_lossy());
    let busy = if src_is_live {
        default_client_pid(&snap).is_some()
    } else {
        !snap.pids_by_dir(&src_base).is_empty()
    };
    if busy {
        return Err(format!(
            "源目录还有客户端在跑（{}），先把它关掉再灌 —— 边写边拷会拷出坏库",
            src_base
        ));
    }
    let dst = PathBuf::from(&slot.user_data_dir);
    let detail = copy_history(&dst, &src_root)?;
    flowlog::log(
        "slots",
        "seed-history",
        &format!("slot={id} from={} got={detail}", src_root.display()),
    );
    Ok(detail)
}

// ---------------------------------------------------------------- 视图

/// 纯读：不落盘。第二个返回值 = 有槽的 PID 已失效，调用方应持锁调 `heal_now()`。
pub fn read_views(paths: &Paths) -> (Vec<SlotView>, bool) {
    let slots = load_slots(paths);
    let snap = store::proc_snapshot(false);
    let has_dead = slots
        .iter()
        .any(|s| s.pid != 0 && !s.running(&snap) || s.pid == 0 && s.running(&snap));
    (build_views(paths, &slots, &snap), has_dead)
}

/// 自愈：清理失效 PID。**必须在 STORE_LOCK 内调用**，且重新读盘再写，
/// 避免用外部读到的旧快照覆盖别的写入。
pub fn heal_now(paths: &Paths) -> Result<usize, String> {
    let mut slots = load_slots(paths);
    let snap = store::proc_snapshot(true);
    let mut fixed = 0;
    for s in slots.iter_mut() {
        match s.live_pid(&snap) {
            // 活着但 PID 变了（客户端自重启后重新拉起主进程）→ 把记录改到新 PID
            Some((pid, started)) if pid != s.pid || started != s.pid_started_at => {
                flowlog::log(
                    "slots",
                    "heal-pid",
                    &format!("slot={} old={} new={pid}", s.id, s.pid),
                );
                s.pid = pid;
                s.pid_started_at = started;
                fixed += 1;
            }
            // 记录的 PID 没了，命令行里也找不到这个数据目录 → 真的关了
            None if s.pid != 0 => {
                flowlog::log(
                    "slots",
                    "heal",
                    &format!("slot={} pid={} started={}", s.id, s.pid, s.pid_started_at),
                );
                s.pid = 0;
                s.pid_started_at = 0;
                fixed += 1;
            }
            _ => {}
        }
    }
    if fixed > 0 {
        save_slots(paths, &slots)?;
    }
    Ok(fixed)
}

fn build_views(paths: &Paths, slots: &[Slot], snap: &store::ProcSnapshot) -> Vec<SlotView> {
    let accounts = store::list_accounts(paths).unwrap_or_default();
    let name_of = |aid: &str| accounts.iter().find(|a| a.id == aid).map(|a| a.name.clone());

    let mut out: Vec<SlotView> = slots
        .iter()
        .map(|s| SlotView {
            id: s.id.clone(),
            app_name: s.app_name.clone(),
            user_data_dir: s.user_data_dir.clone(),
            account_id: s.account_id.clone(),
            account_name: s.account_id.as_ref().and_then(|a| name_of(a)),
            running: s.running(snap),
            pid: s.live_pid(snap).map(|(p, _)| p).unwrap_or(s.pid),
            external: false,
        })
        .collect();

    // 外部实例：主进程里扣掉所有槽占用的 PID，剩下的就是用户自己开的
    let slot_pids: Vec<u32> = slots
        .iter()
        .filter_map(|s| s.live_pid(snap).map(|(p, _)| p))
        .filter(|p| *p != 0)
        .collect();
    let external: Vec<u32> = snap
        .mains
        .iter()
        .copied()
        .filter(|p| !slot_pids.contains(p))
        .collect();
    if !external.is_empty() {
        let recorded = default_slot_account(paths);
        let active = recorded
            .as_ref()
            .and_then(|aid| accounts.iter().find(|a| &a.id == aid))
            .or_else(|| {
                store::read_live(paths)
                    .ok()
                    .flatten()
                    .map(|v| zcrypto::account_identity(&v, paths.home.as_path()))
                    .and_then(|li| {
                        if li.user_id.is_none() && li.username.is_none() && li.email.is_none() {
                            return None;
                        }
                        accounts.iter().find(|a| identity_match(&li, a, paths))
                    })
            });
        out.push(SlotView {
            id: "external".into(),
            app_name: "ZCode".into(),
            user_data_dir: String::new(),
            account_id: active.map(|a| a.id.clone()),
            account_name: active.map(|a| a.name.clone()),
            running: true,
            pid: external.first().copied().unwrap_or(0),
            external: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Paths;

    fn temp_paths(tag: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "zsw-slots-test-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(dir.join(".zcode-switch")).unwrap();
        Paths {
            home: dir.clone(),
            data_root: dir.join("data"),
        }
    }

    fn sample_slot(id: &str) -> Slot {
        Slot {
            id: id.into(),
            app_name: format!("ZCodeSlot{id}"),
            user_data_dir: format!("C:\\x\\slot{id}"),
            account_id: Some("a1".into()),
            device_mid: Some("m1".into()),
            pid: 0,
            pid_started_at: 1234,
            data_epoch: 1,
            created_at: 1,
            last_used_at: 2,
        }
    }

    const LEGACY_JSON: &str = r#"[
      {"id":"1","app_name":"ZCodeSlot1","user_data_dir":"C:\\x\\slot1","account_id":"a1","device_mid":"m1","pid":0,"created_at":1},
      {"id":"2","app_name":"ZCodeSlot2","user_data_dir":"C:\\x\\slot2","account_id":null,"device_mid":"m2","pid":123,"created_at":2}
    ]"#;

    /// 客户端自重启后 PID 变了，但命令行里的数据目录没变 —— 槽必须仍算「在跑」，
    /// 否则会允许同一账号再开一个窗口，两个窗口共用一个数据根。
    #[test]
    fn slot_is_running_even_after_client_restart() {
        let mut slot = sample_slot("1");
        slot.pid = 111;
        slot.pid_started_at = 500;
        let snap = store::ProcSnapshot {
            degraded: false,
            mains: vec![222],
            pids: vec![store::PidInfo {
                pid: 222,
                started_ms: 900,
                data_dir: store::normalize_dir(r"C:\X\slot1"),
            }],
        };
        assert!(slot.running(&snap), "旧 PID 已失效，但同目录进程还在");
        assert_eq!(slot.live_pid(&snap), Some((222, 900)), "heal 应把记录改到新 PID");

        // 进程彻底没了 → 不算在跑，live_pid 为空
        let empty = store::ProcSnapshot::default();
        assert!(!slot.running(&empty));
        assert_eq!(slot.live_pid(&empty), None);

        // 只有无关目录的进程 → 不能误判成在跑
        let other = store::ProcSnapshot {
            degraded: false,
            mains: vec![333],
            pids: vec![store::PidInfo {
                pid: 333,
                started_ms: 1,
                data_dir: store::normalize_dir(r"C:\x\slot9"),
            }],
        };
        assert!(!slot.running(&other));
    }

    /// apply_credentials 绝不能碰 live 根的 deviceMid（撞车根因），
    /// 槽的 mid 只能落在槽自己的数据根里。
    #[test]
    fn apply_credentials_leaves_live_mid_alone() {
        let paths = temp_paths("livesafe");
        let v2 = paths.data_root.join(".zcode").join("v2");
        fs::create_dir_all(&v2).unwrap();
        let live_file = v2.join("telemetry-state.json");
        fs::write(&live_file, r#"{"deviceMid":"LIVE-MID"}"#).unwrap();
        let mut slot = sample_slot("1");
        slot.account_id = None;
        slot.device_mid = Some("SLOT-MID".into());
        slot.user_data_dir = paths.home.join("slot1").to_string_lossy().to_string();
        apply_credentials(&paths, &mut slot).unwrap();
        // live 根原样不动
        let live: Value =
            serde_json::from_str(&fs::read_to_string(&live_file).unwrap()).unwrap();
        assert_eq!(live["deviceMid"], json!("LIVE-MID"));

        // 有账号绑定时也不能把凭据写进 live 根：写进去等于悄悄换掉默认实例的账号，
        // ZCode 的会话列表跟着账号走 → 用户会以为「几个工程的会话全丢了」。
        let live_creds = v2.join("credentials.json");
        fs::write(&live_creds, r#"{"keep":"OLD-LIVE-CREDS"}"#).unwrap();
        {
            let _g = crate::store_guard();
            let acc = store::Account {
                id: "acc-x".into(),
                name: "x".into(),
                created_at: "0".into(),
                updated_at: "0".into(),
                hash: "h".into(),
                credentials: json!({"oauth:zai:access_token":"SLOT-ACC-TOKEN"}),
                config: None,
                virtual_device_mid: None,
                virtual_arms_uid: None,
            };
            store::save_account(&paths, &acc).unwrap();
        }
        slot.account_id = Some("acc-x".into());
        apply_credentials(&paths, &mut slot).unwrap();
        let still_live = fs::read_to_string(&live_creds).unwrap();
        assert!(
            still_live.contains("OLD-LIVE-CREDS"),
            "槽启动绝不能改写 live 根的凭据，实测: {still_live}"
        );

        // 槽根里是槽自己的值
        let slot_tele = slot_zcode_root(&slot).join("v2").join("telemetry-state.json");
        let sv: Value = serde_json::from_str(&fs::read_to_string(&slot_tele).unwrap()).unwrap();
        assert_eq!(sv["deviceMid"], json!("SLOT-MID"));
        let _ = fs::remove_dir_all(&paths.home);
    }

    /// 默认实例 = 主进程命令行里没有 `--user-data-dir`。它跑着的时候绝不能改写 live 根。
    #[test]
    fn default_instance_detected_by_empty_data_dir() {
        let snap = store::ProcSnapshot {
            degraded: false,
            mains: vec![1, 2],
            pids: vec![
                store::PidInfo {
                    pid: 1,
                    started_ms: 1,
                    data_dir: String::new(),
                },
                store::PidInfo {
                    pid: 2,
                    started_ms: 1,
                    data_dir: store::normalize_dir(r"C:\x\slot1"),
                },
            ],
        };
        assert_eq!(default_client_pid(&snap), Some(1), "空 data_dir 的主进程就是默认实例");
        let only_slot = store::ProcSnapshot {
            degraded: false,
            mains: vec![2],
            pids: vec![snap.pids[1].clone()],
        };
        assert_eq!(default_client_pid(&only_slot), None);
    }

    /// 撞车场景：live 根与在跑的槽用同一 mid（2026-11-14 另一台机器实测），
    /// 重新分配时必须换一支新的，绝不能再落回已被占用的值。
    #[test]
    fn pick_slot_mid_avoids_live_and_running_slots() {
        let paths = temp_paths("midpick");
        let v2 = paths.data_root.join(".zcode").join("v2");
        fs::create_dir_all(&v2).unwrap();
        fs::write(v2.join("telemetry-state.json"), r#"{"deviceMid":"m1"}"#).unwrap();
        let mut slot = sample_slot("1");
        slot.device_mid = Some("m1".into());
        {
            let _g = crate::store_guard();
            save_slots(&paths, &[slot]).unwrap();
        }
        let snap = store::ProcSnapshot {
            degraded: false,
            mains: vec![222],
            pids: vec![store::PidInfo {
                pid: 222,
                started_ms: 900,
                data_dir: store::normalize_dir(r"C:\X\slot1"),
            }],
        };
        let picked = pick_slot_mid(&paths, None, &snap, None);
        assert_ne!(picked, "m1", "不能复用 live / 在跑槽的 mid");
        assert!(!picked.is_empty());
        let _ = fs::remove_dir_all(&paths.home);
    }

    #[test]
    fn legacy_array_is_loaded_without_losing_fields() {        let paths = temp_paths("legacy");
        fs::write(slots_file(&paths), LEGACY_JSON).unwrap();
        let slots = load_slots(&paths);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].id, "1");
        assert_eq!(slots[0].account_id.as_deref(), Some("a1"));
        assert_eq!(slots[0].device_mid.as_deref(), Some("m1"));
        assert_eq!(slots[1].pid, 123);
        // 旧记录没有的新字段应当是默认值，而不是解析失败
        assert_eq!(slots[1].pid_started_at, 0);
        assert_eq!(slots[1].data_epoch, 0);
        assert_eq!(slots[1].last_used_at, 0);
    }

    #[test]
    fn save_writes_v2_and_roundtrips() {
        let paths = temp_paths("roundtrip");
        assert!(load_slots(&paths).is_empty()); // 文件不存在 = 空表
        let slot = sample_slot("7");
        {
            let _g = crate::store_guard();
            save_slots(&paths, std::slice::from_ref(&slot)).unwrap();
        }
        let raw = fs::read_to_string(slots_file(&paths)).unwrap();
        assert!(raw.contains("\"version\": 2"), "落盘应为 v2 格式");
        let back = load_slots(&paths);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "7");
        assert_eq!(back[0].pid_started_at, 1234);
        assert_eq!(back[0].data_epoch, 1);
    }

    #[test]
    fn slot_data_root_gets_own_credentials_and_mid() {
        let paths = temp_paths("seed");
        let mut slot = sample_slot("3");
        let ud = paths.home.join("slot3");
        fs::create_dir_all(&ud).unwrap();
        slot.user_data_dir = ud.to_string_lossy().to_string();

        // 全局只有一份 setting.json 可供播种
        let global_v2 = paths.zcode_dir().join("v2");
        fs::create_dir_all(&global_v2).unwrap();
        fs::write(global_v2.join("setting.json"), "{\"a\":1}").unwrap();

        let creds = json!({"oauth:zai:token": "t1"});
        seed_slot_data_root(&paths, &slot, Some(&creds), "mid-1").unwrap();
        let v2 = slot_zcode_root(&slot).join("v2");
        assert!(fs::read_to_string(v2.join("credentials.json"))
            .unwrap()
            .contains("t1"));
        assert!(fs::read_to_string(v2.join("telemetry-state.json"))
            .unwrap()
            .contains("mid-1"));
        assert!(v2.join("setting.json").exists(), "静态配置应从全局播种");

        // 二次播种：mid 与凭据跟着换，静态配置不被覆盖
        fs::write(v2.join("setting.json"), "{\"a\":2}").unwrap();
        let creds2 = json!({"oauth:zai:token": "t2"});
        seed_slot_data_root(&paths, &slot, Some(&creds2), "mid-2").unwrap();
        assert!(fs::read_to_string(v2.join("telemetry-state.json"))
            .unwrap()
            .contains("mid-2"));
        assert!(fs::read_to_string(v2.join("credentials.json"))
            .unwrap()
            .contains("t2"));
        assert_eq!(
            fs::read_to_string(v2.join("setting.json")).unwrap(),
            "{\"a\":2}"
        );
    }

    #[test]
    fn corrupt_file_is_backed_up_and_original_kept() {
        let paths = temp_paths("corrupt");
        fs::write(slots_file(&paths), "{this is not json").unwrap();
        assert!(load_slots(&paths).is_empty());
        let backups: Vec<String> = fs::read_dir(paths.store_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("slots.json.bad-"))
            .collect();
        assert_eq!(backups.len(), 1, "损坏文件必须留下备份");
        assert!(slots_file(&paths).exists(), "原文件不能被删");
    }

    #[test]
    fn heal_clears_dead_pid_only() {
        let paths = temp_paths("heal");
        let mut dead = sample_slot("1");
        dead.pid = u32::MAX; // 不可能存在的 PID
        let mut alive = sample_slot("2");
        alive.pid = 0;
        {
            let _g = crate::store_guard();
            save_slots(&paths, &[dead, alive]).unwrap();
            assert_eq!(heal_now(&paths).unwrap(), 1);
        }
        let slots = load_slots(&paths);
        assert_eq!(slots[0].pid, 0, "失效 PID 应被清掉");
        assert_eq!(slots[0].pid_started_at, 0);
        assert_eq!(slots[1].pid, 0);
    }

    #[test]
    fn guard_flag_tracks_lock_depth() {
        assert!(!crate::in_store_guard());
        {
            let _g = crate::store_guard();
            assert!(crate::in_store_guard());
        }
        assert!(!crate::in_store_guard());
    }

    /// 复用未运行的槽：条数不增长、编号最小。
    /// **同账号复用不换代**（否则每次多开都换个空数据根，用户的会话历史就「看起来没了」）；
    /// 换账号才换代 —— 旧账号的 Cookies 不能串给新账号。
    #[test]
    fn create_recycles_smallest_id_and_only_rotates_on_account_change() {
        let paths = temp_paths("recycle");
        let mut dirs: Vec<String> = Vec::new();
        let mut epochs: Vec<u32> = Vec::new();
        {
            let _g = crate::store_guard();
            for _ in 0..5 {
                let s = create_slot(&paths, Some("a1".into())).unwrap();
                assert_eq!(s.id, "1", "未运行的槽应被按最小编号复用");
                assert!(std::path::Path::new(&s.user_data_dir).exists(), "目录应已创建");
                dirs.push(s.user_data_dir.clone());
                epochs.push(s.data_epoch);
            }
            // 同账号：一直用同一个数据根（固定落在 profiles/<账号id>）
            assert_eq!(epochs, vec![0, 0, 0, 0, 0], "同账号复用不该换代");
            assert!(
                dirs.iter().all(|d| d == &dirs[0]),
                "同账号复用必须留在同一个数据根里（历史会话就在这里）"
            );
            assert!(
                dirs[0].contains("profiles"),
                "账号的数据根应固定在 profiles 下，实测: {}",
                dirs[0]
            );

            // 换账号：换到那个账号自己的目录，旧目录原样保留（历史不能丢）
            let old = dirs[0].clone();
            let s2 = create_slot(&paths, Some("a2".into())).unwrap();
            assert_eq!(s2.id, "1");
            assert_ne!(s2.user_data_dir, old, "换账号必须换目录");
            assert!(s2.user_data_dir.contains("a2"), "应落在 a2 自己的目录");
            assert!(std::path::Path::new(&old).exists(), "旧目录不能删");

            // 再切回 a1：必须回到最初那个目录（会话还在里面）
            let s3 = create_slot(&paths, Some("a1".into())).unwrap();
            assert_eq!(s3.user_data_dir, old, "切回同一个账号，必须回到原目录");
        }
        assert_eq!(load_slots(&paths).len(), 1, "回收不应让记录增长");
    }

    /// 灌历史：只拷会话相关的东西（索引 + 消息库 + workspace），不碰凭据。
    #[test]
    fn copy_history_brings_sessions_not_credentials() {
        let paths = temp_paths("seedhistory");
        let src = paths.data_root.join(".zcode");
        fs::create_dir_all(src.join("v2")).unwrap();
        fs::create_dir_all(src.join("cli").join("db")).unwrap();
        fs::create_dir_all(src.join("workspace").join("draft-x")).unwrap();
        fs::write(src.join("v2").join("tasks-index.sqlite"), b"INDEX").unwrap();
        fs::write(src.join("cli").join("db").join("db.sqlite"), b"MSGS").unwrap();
        fs::write(src.join("workspace").join("draft-x").join("a.txt"), b"hi").unwrap();
        fs::write(src.join("v2").join("credentials.json"), b"CREDS").unwrap();

        let dst = paths.home.join("profiles").join("acc1");
        let detail = copy_history(&dst, &src).unwrap();
        let out = dst.join(".zcode");
        assert_eq!(fs::read(out.join("v2").join("tasks-index.sqlite")).unwrap(), b"INDEX");
        assert_eq!(fs::read(out.join("cli").join("db").join("db.sqlite")).unwrap(), b"MSGS");
        assert!(out.join("workspace").join("draft-x").join("a.txt").exists());
        assert!(
            !out.join("v2").join("credentials.json").exists(),
            "灌历史绝不能把凭据一起拷进去（会把两个账号串成一个）"
        );
        assert!(detail.contains("tasks-index.sqlite"), "返回值应说明拷了什么");

        // 源目录没有索引 → 明确报错，不能建出一个空壳
        let empty_src = paths.home.join("emptyroot");
        fs::create_dir_all(empty_src.join(".zcode")).unwrap();
        assert!(copy_history(&paths.home.join("x"), &empty_src.join(".zcode")).is_err());

        let _ = fs::remove_dir_all(&paths.home);
    }

    #[test]
    fn store_guard_is_reentrant() {
        // 1.7.5 基线这里会自锁：锁内再取同一把锁 → 线程永久阻塞。
        let outer = crate::store_guard();
        assert!(crate::in_store_guard());
        let inner = crate::store_guard();
        assert!(crate::in_store_guard());
        drop(inner);
        assert!(crate::in_store_guard(), "内层释放不能清掉外层计数");
        drop(outer);
        assert!(!crate::in_store_guard());
    }

    #[test]
    fn gc_removes_idle_slots_only() {
        let paths = temp_paths("gc");
        {
            let _g = crate::store_guard();
            let mut slots = Vec::new();
            for i in 1..=3u32 {
                let mut s = sample_slot(&i.to_string());
                s.pid = 0; // 未运行
                slots.push(s);
            }
            save_slots(&paths, &slots).unwrap();
            assert_eq!(gc(&paths, 0).unwrap(), 3);
        }
        assert!(load_slots(&paths).is_empty());
    }
}
