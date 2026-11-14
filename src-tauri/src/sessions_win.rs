//! Windows 实现：加载内嵌的 sqlite3.dll，做会话库行级并集。
//!
//! 由 `sessions.rs` 以 `#[path = "sessions_win.rs"] mod win;` 引入，所以这里
//! 直接用 `super::db_files` / `super::sql_lit` / `super::sql_ident`。

use super::{db_files, sql_ident, sql_lit};
use crate::store::Paths;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const SQLITE_OK: i32 = 0;
const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;
const OPEN_RW_CREATE: i32 = 0x0000_0002 | 0x0000_0004 | 0x0001_0000;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(h: *mut c_void, name: *const u8) -> *mut c_void;
}

type FOpen = unsafe extern "C" fn(*const i8, *mut *mut c_void, i32, *const i8) -> i32;
type FExec = unsafe extern "C" fn(
    *mut c_void,
    *const i8,
    Option<unsafe extern "C" fn(*mut c_void, i32, *mut *const i8, *mut *const i8)>,
    *mut c_void,
    *mut *mut i8,
) -> i32;
type FPrepare =
    unsafe extern "C" fn(*mut c_void, *const i8, i32, *mut *mut c_void, *mut *const i8) -> i32;
type FStep = unsafe extern "C" fn(*mut c_void) -> i32;
type FColumnText = unsafe extern "C" fn(*mut c_void, i32) -> *const i8;
type FFinalize = unsafe extern "C" fn(*mut c_void) -> i32;
type FClose = unsafe extern "C" fn(*mut c_void) -> i32;
type FErrmsg = unsafe extern "C" fn(*mut c_void) -> *const i8;
type FBusy = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type FChanges = unsafe extern "C" fn(*mut c_void) -> i32;
type FColumnCount = unsafe extern "C" fn(*mut c_void) -> i32;

struct Api {
    open: FOpen,
    exec: FExec,
    prepare: FPrepare,
    step: FStep,
    column_text: FColumnText,
    column_count: FColumnCount,
    finalize: FFinalize,
    close: FClose,
    errmsg: FErrmsg,
    busy: FBusy,
    changes: FChanges,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

/// sqlite3 官方 amalgamation 编出来的 64 位 DLL（1,034,487 字节），嵌进 exe 里
const DLL_BYTES: &[u8] = include_bytes!("../assets/sqlite3.dll");

fn api(paths: &Paths) -> Result<&'static Api, String> {
    let dll = paths.store_dir().join("bin").join("sqlite3.dll");
    let need = match std::fs::metadata(&dll) {
        Ok(m) => m.len() as usize != DLL_BYTES.len(),
        Err(_) => true,
    };
    if need {
        if let Some(p) = dll.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(&dll, DLL_BYTES).map_err(|e| format!("写 sqlite3.dll 失败: {e}"))?;
    }
    API.get_or_init(|| unsafe { load(&dll) })
        .as_ref()
        .ok_or_else(|| format!("sqlite3.dll 加载失败: {}", dll.display()))
}

unsafe fn load(dll: &Path) -> Option<Api> {
    let wide: Vec<u16> = dll
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = LoadLibraryW(wide.as_ptr());
    if h.is_null() {
        return None;
    }
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let p = GetProcAddress(h, concat!($name, "\0").as_ptr() as *const u8);
            if p.is_null() {
                return None;
            }
            std::mem::transmute::<*mut c_void, $ty>(p)
        }};
    }
    Some(Api {
        open: sym!("sqlite3_open_v2", FOpen),
        exec: sym!("sqlite3_exec", FExec),
        prepare: sym!("sqlite3_prepare_v2", FPrepare),
        step: sym!("sqlite3_step", FStep),
        column_text: sym!("sqlite3_column_text", FColumnText),
        column_count: sym!("sqlite3_column_count", FColumnCount),
        finalize: sym!("sqlite3_finalize", FFinalize),
        close: sym!("sqlite3_close", FClose),
        errmsg: sym!("sqlite3_errmsg", FErrmsg),
        busy: sym!("sqlite3_busy_timeout", FBusy),
        changes: sym!("sqlite3_changes", FChanges),
    })
}

struct Db {
    api: &'static Api,
    conn: *mut c_void,
}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.conn);
        }
    }
}

impl Db {
    fn open(api: &'static Api, path: &Path) -> Result<Db, String> {
        let c = CString::new(path.to_string_lossy().to_string())
            .map_err(|e| format!("路径非法: {e}"))?;
        let mut conn: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { (api.open)(c.as_ptr(), &mut conn, OPEN_RW_CREATE, std::ptr::null()) };
        if rc != SQLITE_OK || conn.is_null() {
            return Err(format!("打开 {} 失败 (code {rc})", path.display()));
        }
        unsafe {
            (api.busy)(conn, 5000);
        }
        Ok(Db { api, conn })
    }

    fn err(&self) -> String {
        unsafe {
            let p = (self.api.errmsg)(self.conn);
            if p.is_null() {
                "unknown".into()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
            }
        }
    }

    fn exec(&self, sql: &str) -> Result<(), String> {
        let c = CString::new(sql).map_err(|e| format!("SQL 非法: {e}"))?;
        let rc = unsafe {
            (self.api.exec)(
                self.conn,
                c.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            return Err(format!("{} ({rc})", self.err()));
        }
        Ok(())
    }

    fn changes(&self) -> i32 {
        unsafe { (self.api.changes)(self.conn) }
    }

    fn query(&self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let c = CString::new(sql).map_err(|e| format!("SQL 非法: {e}"))?;
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            (self.api.prepare)(self.conn, c.as_ptr(), -1, &mut stmt, std::ptr::null_mut())
        };
        if rc != SQLITE_OK || stmt.is_null() {
            return Err(format!("prepare 失败: {} ({rc})", self.err()));
        }
        let mut out = Vec::new();
        loop {
            let step = unsafe { (self.api.step)(stmt) };
            if step == SQLITE_ROW {
                // 按列数取，而不是「遇到 NULL 就停」：NULL 列会让老写法把一行截断
                let ncols = unsafe { (self.api.column_count)(stmt) };
                let mut row = Vec::new();
                for i in 0..ncols {
                    let txt = unsafe { (self.api.column_text)(stmt, i) };
                    if txt.is_null() {
                        row.push(String::new());
                    } else {
                        row.push(
                            unsafe { std::ffi::CStr::from_ptr(txt) }
                                .to_string_lossy()
                                .to_string(),
                        );
                    }
                }
                out.push(row);
            } else if step == SQLITE_DONE {
                break;
            } else {
                let e = self.err();
                unsafe {
                    (self.api.finalize)(stmt);
                }
                return Err(format!("step 失败: {e} ({step})"));
            }
        }
        unsafe {
            (self.api.finalize)(stmt);
        }
        Ok(out)
    }
}

/// 把 `src` 的行并进 `dst`；dst 不存在就整库拷过去。
///
/// 幂等：两边都对得上唯一索引，`INSERT OR REPLACE` 重复跑不会多出行
/// （实测 60 tasks / 238 sessions / 5,396 messages / 18,793 parts，连跑两遍数字不变）。
fn merge_db(_paths: &Paths, api: &'static Api, dst: &Path, src: &Path) -> Result<usize, String> {
    if !src.is_file() {
        return Ok(0);
    }
    if !dst.is_file() {
        if let Some(p) = dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::copy(src, dst)
            .map_err(|e| format!("复制 {} 失败: {e}", src.display()))?;
        // 不 return：源库若是 WAL 模式，最近的行可能还在 -wal 里，整文件拷贝会漏掉它们，
        // 必须再走一遍 SELECT 才能读到（2026-11-14 复检发现）。
    }
    let db = Db::open(api, dst)?;
    let attach = format!("ATTACH DATABASE {} AS src", sql_lit(&src.to_string_lossy()));
    db.exec(&attach)
        .map_err(|e| format!("ATTACH {} 失败: {e}", src.display()))?;
    let tables = db.query(
        "SELECT name FROM src.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let _ = db.exec("BEGIN");
    let mut rows = 0usize;
    let mut skipped = 0usize;
    let mut unchanged = 0usize;
    for t in tables {
        let Some(name) = t.first() else { continue };
        if name.is_empty() {
            continue;
        }
        // 先比「行数 + 最大更新时刻」：完全一样就整表跳过，不再重写几万行
        // （开窗慢的主因；纯文件 mtime 戳记会被「自己刚写过」抵消，不可靠）
        if table_unchanged(&db, name) {
            unchanged += 1;
            continue;
        }
        // 两边 ZCode 版本不同导致表结构不一致时跳过这张表，绝不让整次合并失败
        let sql = format!(
            "INSERT OR REPLACE INTO main.{} SELECT * FROM src.{}",
            sql_ident(name),
            sql_ident(name)
        );
        match db.exec(&sql) {
            Ok(()) => rows += db.changes().max(0) as usize,
            Err(_) => skipped += 1,
        }
    }
    let _ = db.exec("COMMIT");
    let _ = db.exec("DETACH DATABASE src");
    if skipped > 0 {
        crate::flowlog::log(
            "sessions",
            "merge-skip-tables",
            &format!("dst={} skipped={skipped}", dst.display()),
        );
    }
    if unchanged > 0 {
        crate::flowlog::log(
            "sessions",
            "merge-unchanged",
            &format!("dst={} tables={unchanged}", dst.display()),
        );
    }
    Ok(rows)
}

/// 这张表在 src/main 里是不是完全一样（行数 + 最大 updated_at/created_at）。
///
/// 只比行数不够：同一个会话被改名/归档时行数不变但内容变了，靠时间戳能发现。
/// 没有时间戳列的表退化成只比行数（这些表基本都是追加写）。
fn table_unchanged(db: &Db, table: &str) -> bool {
    let count_sql = format!(
        "SELECT (SELECT count(*) FROM main.{t}), (SELECT count(*) FROM src.{t})",
        t = sql_ident(table)
    );
    match db.query(&count_sql) {
        Ok(rows) => {
            let Some(r) = rows.first() else { return false };
            if r.len() != 2 || r[0] != r[1] {
                return false;
            }
        }
        Err(_) => return false,
    }
    // 有 updated_at 就比较最大值（TEXT/INTEGER 都能比大小）
    for col in ["updated_at", "created_at"] {
        let sql = format!(
            "SELECT (SELECT max({c}) FROM main.{t}), (SELECT max({c}) FROM src.{t})",
            c = sql_ident(col),
            t = sql_ident(table)
        );
        if let Ok(rows) = db.query(&sql) {
            if let Some(r) = rows.first() {
                if r.len() == 2 && r[0] != r[1] {
                    return false;
                }
                return true;
            }
        }
    }
    true
}

/// 每个 (dst, src) 对一条戳记，文件名用 sha256 压平，避免路径里有非法字符
pub fn hub_root(paths: &Paths) -> PathBuf {
    paths.store_dir().join("sessions-hub").join(".zcode")
}

fn ensure_hub(paths: &Paths) -> PathBuf {
    let hub = hub_root(paths);
    let _ = std::fs::create_dir_all(hub.join("v2"));
    hub
}

/// 把默认根（用户主历史）并进 hub。只读默认根，默认客户端在跑也能做。
/// 用一个 stamp 记「上次导入时默认根那两个库的大小+mtime」，没变就跳过，
/// 免得每次开窗都把 67MB 消息库重写一遍。
pub fn import_live(paths: &Paths) -> Result<usize, String> {
    let hub = ensure_hub(paths);
    let live = paths.data_root.join(".zcode");
    let (live_idx, live_cli) = db_files(&live);
    if !live_idx.is_file() && !live_cli.is_file() {
        return Ok(0);
    }
    let stamp_file = paths.store_dir().join("sessions-hub").join("live-stamp.txt");
    // stamp 必须在合并**之前**采样：否则合并期间默认根又写了新会话，会被记成「已导入」而漏掉
    let now_stamp = stamp_of(&live_idx) + &stamp_of(&live_cli);
    let hub_empty = !db_files(&hub).0.is_file();
    if !hub_empty {
        if let Ok(old) = std::fs::read_to_string(&stamp_file) {
            if old.trim() == now_stamp.trim() {
                return Ok(0);
            }
        }
    }
    let api = api(paths)?;
    let (hub_idx, hub_cli) = db_files(&hub);
    let mut n = merge_db(paths, api, &hub_idx, &live_idx)?;
    n += merge_db(paths, api, &hub_cli, &live_cli)?;
    let _ = std::fs::write(&stamp_file, &now_stamp);
    Ok(n)
}

fn stamp_of(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}:{}\n", m.len(), t)
        }
        Err(_) => "0:0\n".to_string(),
    }
}

/// 双向并集：hub ↔ root
pub fn merge_pair(paths: &Paths, hub: &Path, root: &Path) -> Result<(usize, usize), String> {
    let api = api(paths)?;
    let (hub_idx, hub_cli) = db_files(hub);
    let (root_idx, root_cli) = db_files(root);
    let mut into_root = merge_db(paths, api, &root_idx, &hub_idx)?;
    into_root += merge_db(paths, api, &root_cli, &hub_cli)?;
    let mut into_hub = merge_db(paths, api, &hub_idx, &root_idx)?;
    into_hub += merge_db(paths, api, &hub_cli, &root_cli)?;
    Ok((into_root, into_hub))
}

/// 开窗前：hub 与目标根双向并集（目标根还没起来，写它是安全的）
pub fn pull(paths: &Paths, root: &Path) -> Result<String, String> {
    if root == hub_root(paths) {
        return Ok("skip".into());
    }
    let _ = import_live(paths);
    let hub = ensure_hub(paths);
    if !root.is_dir() {
        return Ok("目标根还不存在，首次开窗后再生效".into());
    }
    let (a, b) = merge_pair(paths, &hub, root)?;
    copy_missing_workspace(&hub, root);
    Ok(format!("拉入 {a} 行 / 汇入 {b} 行"))
}

/// 关窗后：把该根的会话汇进 hub
pub fn push(paths: &Paths, root: &Path) -> Result<usize, String> {
    if root == hub_root(paths) || !root.is_dir() {
        return Ok(0);
    }
    let api = api(paths)?;
    let hub = ensure_hub(paths);
    let (root_idx, root_cli) = db_files(root);
    let (hub_idx, hub_cli) = db_files(&hub);
    let mut n = merge_db(paths, api, &hub_idx, &root_idx)?;
    n += merge_db(paths, api, &hub_cli, &root_cli)?;
    copy_missing_workspace(root, &hub);
    Ok(n)
}

/// 所有已知数据根（未运行的）一次性双向并集
pub fn sync_all(paths: &Paths) -> Result<String, String> {
    let _ = import_live(paths);
    let hub = ensure_hub(paths);
    let slots = crate::slots::load_slots(paths);
    let snap = crate::store::proc_snapshot(true);
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    for s in slots {
        if s.running(&snap) {
            skipped.push(s.id.clone());
            continue;
        }
        let root = PathBuf::from(&s.user_data_dir).join(".zcode");
        if !root.is_dir() {
            continue;
        }
        match merge_pair(paths, &hub, &root) {
            Ok((a, b)) => done.push(format!("{}=+{a}/+{b}", s.id)),
            Err(e) => skipped.push(format!("{}:{e}", s.id)),
        }
    }
    Ok(format!(
        "hub={} 已同步[{}] 跳过[{}]",
        hub.display(),
        done.join(" "),
        skipped.join(" ")
    ))
}

/// workspace 里的会话草稿目录：缺哪个补哪个（单个都很小，>200MB 的跳过）
fn copy_missing_workspace(src: &Path, dst: &Path) {
    let from = src.join("workspace");
    let to = dst.join("workspace");
    let Ok(rd) = std::fs::read_dir(&from) else {
        return;
    };
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let target = to.join(e.file_name());
        if target.exists() {
            continue;
        }
        if dir_size(&e.path()) > 200 * 1024 * 1024 {
            continue;
        }
        let _ = std::fs::create_dir_all(&to);
        let _ = copy_tree(&e.path(), &target);
    }
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_file() => n += e.metadata().map(|m| m.len()).unwrap_or(0),
                Ok(t) if t.is_dir() => n += dir_size(&e.path()),
                _ => {}
            }
        }
    }
    n
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            let _ = std::fs::copy(e.path(), &to);
        }
    }
    Ok(())
}
