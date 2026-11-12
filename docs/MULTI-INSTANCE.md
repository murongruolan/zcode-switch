# 多开（同一账号多客户端）机制说明

更新时间：2026-11-13（对应 `slots.rs` v2 落盘 + 启动闸门 + **`ZCODE_DATA_BASE_DIR` 数据根隔离**）
适用范围：`zcode-switch-175w`（1.7.5-Z.EMAIL + 多开）

---

## 1. 为什么需要多开

ZCode 服务端按**身份**计 subagent 并发（实测 3 个），超额返回
`rate_limited / "user concurrency limit exceeded"`。
同一个账号在多个客户端里干活，只要它们被服务端认成**同一个身份**，就共享这 3 个名额。

于是「多开」要同时做到三件事，缺一不可：

| 要隔离的东西 | 靠什么隔离 |
|---|---|
| 单实例锁 / userData | `ZCODE_DESKTOP_APPLICATION_NAME`、`--user-data-dir=<槽目录>` |
| **`.zcode` 数据根（凭据 / telemetry / 会话库 / 日志）** | **`ZCODE_DATA_BASE_DIR=<槽目录>`** |
| 设备身份（device_mid） | 播种到 `<槽目录>\.zcode\v2\telemetry-state.json`，并用 `ZCODE_TELEMETRY_DEVICE_MID` 双保险 |

只隔离 userData、不隔离数据根，两个客户端读到的还是同一份 `credentials.json`
与同一个 `telemetry-state.json` —— 服务端当然按同一个身份算额度，这是上一版踩过的坑。

> 数据根的开关是 `getDataBaseDir()`：`显式设置 || env.ZCODE_DATA_BASE_DIR || HOME`，
> 其下取 `<base>\.zcode`。`ZCODE_HOME` 实测**不生效**，不要用。

---

## 2. 一个「槽」到底是什么

一个槽 = 一份独立 userData 目录 + 一个独立 device_mid + 一个已登录账号 + 一个进程 PID。

```json
{
  "version": 2,
  "slots": [
    {
      "id": "3",
      "app_name": "ZCodeSlot3",
      "user_data_dir": "C:\\Users\\<你>\\.zcode-switch\\slots\\slot3-e1-1a2b3c4d",
      "account_id": "…",
      "device_mid": "…",
      "pid": 12345,
      "pid_started_at": 1793980000000,
      "data_epoch": 1,
      "created_at": 1793970000,
      "last_used_at": 1793980000
    }
  ]
}
```

- `pid_started_at`：进程创建时间，用来区分「PID 被系统复用给了别的进程」。
  PID 相同但创建时间差 > 2s → 视为不是同一个进程，槽算「没在跑」。
- `data_epoch`：userData 目录轮换代数。**复用槽或换账号都 +1 并换新目录**，
  否则新账号会继承旧账号的 Cookies / LocalStorage。
- 旧的「无版本数组」格式仍可读，首次写入自动升级成 v2。

---

## 3. 启动时到底做了什么

1. 拿启动闸门（`slots::launch_guard()`）——所有启动动作串行，默认客户端也一样。
2. `apply_credentials()`：
   - 把该账号凭据写进 live `~/.zcode/v2/credentials.json`（给「默认客户端」用的兜底）；
   - **同时**把凭据与 `device_mid` 播种到 `<槽目录>\.zcode\v2\`（`credentials.json` / `telemetry-state.json`），
     配置类文件（`provider_config.json` / `setting.json` / `onboarding-record.json` / `bot-*.json`）只补缺不覆盖。
3. spawn 客户端，带 `--user-data-dir=<槽目录>` 与：
   - `ZCODE_DESKTOP_APPLICATION_NAME`、`ZCODE_DESKTOP_USER_DATA_DIR`、`ZCODE_DESKTOP_SESSION_DATA_DIR`；
   - `ZCODE_DESKTOP_HOME_DIR=<槽目录>`（桌面侧配置也落在同一棵树）；
   - **`ZCODE_DATA_BASE_DIR=<槽目录>`**（一切数据根隔离的关键）；
   - **`ZCODE_TELEMETRY_DEVICE_MID=<该槽 mid>`**（不依赖文件读取时机）。
4. 等「落地」：轮询该槽 userData 下是否出现 `Preferences` / `Local State`
   且 mtime ≥ 启动时刻（100ms 一次，最多 6s）。拿到证据再放闸门。
   - `settle=userdata`：正常；
   - `settle=timeout`：6s 没等到，闸门照样放行，但值得查一下进程是否真起来了。
5. 记录 PID / 创建时间 / `last_used_at`，写回 `slots.json`；
   flowlog 记 `isolated=<槽目录>\.zcode\v2 是否为目录`，用来事后确认这一发到底走没走结构隔离。

**为什么还要串行**：live 文件（默认客户端那份）仍是全局单份，两个启动重叠时后写的会覆盖前一个
还没读到的值。槽客户端已经只读自己的数据根，所以串行主要是保护默认客户端 + 让 flowlog 可读。

---

## 4. 编号与回收

- 上限 32 个槽（真正约束是内存，每个客户端约 1 GB）。
- 新建时优先复用**任何没在跑的槽**，取编号最小的那个。
  （早期版本只回收「未运行且没绑账号」的槽，9 条老记录会把 32 个编号吃光，然后假报「槽位已满」。）
- 「清理未运行的槽」（`slots_gc`）只删 `slots.json` 里的记录，**不删磁盘目录**。

命令行（不开界面也能查/清）：

```powershell
zcode-switch.exe --cli slots                     # 只读：槽表 + 视图 + 是否有失效 PID
zcode-switch.exe --cli slots-gc --secs 0         # 清掉所有未运行的槽记录
zcode-switch.exe --cli slots-gc --secs 86400     # 只清闲置超过一天的
```

---

## 5. 界面上的三态

| 状态 | 按钮 | 说明 |
|---|---|---|
| 该账号正在某个客户端里跑 | 「已登录」（禁用） | 同一账号不能被两个客户端同时占着 |
| 当前生效账号、但没在跑 | 「当前」 | 点开可以给它多开一个独立客户端 |
| 其它 | 「切换」 | 点开选一个运行中的客户端，或就地多开 |

多开气泡里，每个运行中的客户端右侧有「关闭」—— 只关这一个，不影响其它客户端。
macOS 上不展示多开相关入口（后端 `slots_capability()` 返回 false）。

---

## 6. 排障

| 现象 | 先看什么 |
|---|---|
| 多开点了没反应 | flowlog `slots` 流的 `launch` 行有没有 `pid=`；`--cli slots` 里该槽 `running` 是否 true |
| 两个账号额度还是串 | 先看 flowlog `launch` 行有没有 `isolated=true`；再看两个槽各自 `\.zcode\v2\telemetry-state.json` 的 `deviceMid` 与 `credentials.json` 是否不同 |
| 界面显示在跑但关不掉 | `--cli slots` 看 `pid_started_at` 是否匹配；PID 被复用时旧记录会被自愈清掉 |
| 报「槽位上限 32」 | `--cli slots` 看记录数；多出来的是没清理的僵尸记录，跑一次 `slots-gc --secs 0` |
| 新账号登录后还是旧账号 | 检查 `data_epoch` 是否 +1、`user_data_dir` 是否换成新目录 |

`slots.json` 解析失败时不会崩：会先写 `slots.json.bad-<时间戳>`，再按空表继续，原文件保留。

---

## 7. 验证方法（额度是否真的解耦）

1. 用「多开」给两个不同账号各开一个客户端（各自的数据根不同）。
2. 每个客户端在**同一分钟内**各发一个「用 4 个 subagent 并行」的任务。
3. 查 `~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表（`started_at` 是毫秒）：
   - 期望每个客户端 3 个在跑、1 个 `user concurrency limit exceeded`，合计 ≥6 个并发；
   - 若合计被封在 3，说明两端仍被当成同一身份 → 回第 6 节第二条，先核 `deviceMid` 与 `credentials.json` 是否各在各的槽目录。

单端上限（实测，四次复现）：一个客户端一次发 4 个 subagent，必然 3 个成功、1 个被拒，重试即可成功。
