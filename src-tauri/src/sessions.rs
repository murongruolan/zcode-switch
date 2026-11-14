//! 会话共享（跨账号窗口的同一份会话）
//!
//! 背景：ZCode 的**凭据**（`<数据根>\.zcode\v2\credentials.json`）和**会话库**
//! （`<数据根>\.zcode\v2\tasks-index.sqlite` + `<数据根>\.zcode\cli\db\db.sqlite`）
//! 都长在同一个数据根里。2026-11-14 实测确认：
//!
//! ```text
//! ZCODE_DATA_BASE_DIR=A   →  A\.zcode\v2\credentials.json   （客户端真正读写的凭据）
//! ZCODE_DESKTOP_HOME_DIR=B →  B\.zcode\v2\setting.json       （只放桌面配置）
//! ```
//!
//! 也就是说「共享数据根 = 共享登录身份」，环境变量里没有把两者拆开的开关；而各窗口要
//! 用各自的账号（各自的并发额度），就必须各自独立数据根。于是会话只能**在文件层做并集**：
//! 每个窗口启动前、关闭后，把它的会话库与「并集中心」(`<store>\sessions-hub\.zcode`)
//! 双向 `INSERT OR REPLACE` 一遍。
//!
//! 合并按表做：`ATTACH` 源库 → 遍历 `src.sqlite_master` 的表 →
//! `INSERT OR REPLACE INTO main."t" SELECT * FROM src."t"`。主键相同即同一行，
//! 因此重复合并不会叠出多份；两边不写同一条会话时也不会互相覆盖。
//!
//! SQLite 引擎是 sqlite3 官方 amalgamation 编出来的 `sqlite3.dll`（嵌进 exe，首用时释放），
//! 不引入需要 C 编译器参与构建的 crate。

#[allow(unused_imports)]
use crate::store::Paths;
use std::path::{Path, PathBuf};

/// 会话库在某个数据根（`<base>\.zcode`）下的两处位置
pub(crate) fn db_files(zcode_root: &Path) -> (PathBuf, PathBuf) {
    (
        zcode_root.join("v2").join("tasks-index.sqlite"),
        zcode_root.join("cli").join("db").join("db.sqlite"),
    )
}

/// SQL 单引号字面量转义
pub(crate) fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// SQL 双引号标识符转义
pub(crate) fn sql_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(windows)]
#[path = "sessions_win.rs"]
mod win;

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
pub fn hub_root(_paths: &Paths) -> PathBuf {
    PathBuf::new()
}

#[cfg(not(windows))]
pub fn pull(_paths: &Paths, _root: &Path) -> Result<String, String> {
    Err("会话共享目前只在 Windows 上实现".into())
}

#[cfg(not(windows))]
pub fn push(_paths: &Paths, _root: &Path) -> Result<usize, String> {
    Err("会话共享目前只在 Windows 上实现".into())
}

#[cfg(not(windows))]
pub fn import_live(_paths: &Paths) -> Result<usize, String> {
    Err("会话共享目前只在 Windows 上实现".into())
}

#[cfg(not(windows))]
pub fn sync_all(_paths: &Paths) -> Result<String, String> {
    Err("会话共享目前只在 Windows 上实现".into())
}

#[cfg(not(windows))]
pub fn stats(_paths: &Paths) -> Result<serde_json::Value, String> {
    Err("会话共享目前只在 Windows 上实现".into())
}
