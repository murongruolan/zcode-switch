# 多开（同一账号多客户端）机制说明

更新时间：2026-11-13（对应 `slots.rs` v2 落盘 + 启动闸门改造）
适用范围：`zcode-switch-175w`（1.7.5-Z.EMAIL + 多开）

---

## 1. 为什么需要多开

ZCode 服务端按**账号**计 subagent 并发（约 3 个），超额返回
`rate_limited / "user concurrency limit exceeded"`。
一个账号在多个客户端里同时干活，只要它们被服务端认成**同一台设备**，就共享这 3 个名额。

于是「多开」要同时做到两件事，缺一不可：

| 要隔离的东西 | 靠什么隔离 |
|---|---|
| 单实例锁 / userData | `ZCODE_DESKTOP_APPLICATION_NAME`、`--user-data-dir=<槽目录>` |
| 设备身份（device_mid） | 每个客户端写不同的 `telemetry-state.json` 值 |

只隔离 userData、不隔离 device_mid，服务端仍会把它们合并成一台设备 —— 这是上一版踩过的坑。

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
2. 把该账号的凭据写进 live `~/.zcode/v2/credentials.json`，把该槽的 device_mid
   写进 live `~/.zcode/v2/telemetry-state.json`（ZCode **只在启动时读一次**）。
3. spawn 客户端，带 `--user-data-dir` 与四个 `ZCODE_DESKTOP_*` 环境变量。
4. 等「落地」：轮询该槽 userData 下是否出现 `Preferences` / `Local State`
   且 mtime ≥ 启动时刻（100ms 一次，最多 6s）。拿到证据再放闸门。
   - `settle=userdata`：正常；
   - `settle=timeout`：6s 没等到，闸门照样放行，但值得查一下进程是否真起来了。
5. 记录 PID / 创建时间 / `last_used_at`，写回 `slots.json`。

**为什么必须串行**：live 文件是全局单份，两个启动重叠时，后启动的 mid
会覆盖前一个「还没读到」的值，两个客户端就变成同一台设备，额度重新串在一起。

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
| 两个账号额度还是串 | 两个槽的 `device_mid` 是否不同；`launch` 行里的 `mid` 是否就是 `slots.json` 里的那个 |
| 界面显示在跑但关不掉 | `--cli slots` 看 `pid_started_at` 是否匹配；PID 被复用时旧记录会被自愈清掉 |
| 报「槽位上限 32」 | `--cli slots` 看记录数；多出来的是没清理的僵尸记录，跑一次 `slots-gc --secs 0` |
| 新账号登录后还是旧账号 | 检查 `data_epoch` 是否 +1、`user_data_dir` 是否换成新目录 |

`slots.json` 解析失败时不会崩：会先写 `slots.json.bad-<时间戳>`，再按空表继续，原文件保留。

---

## 7. 验证方法（额度是否真的解耦）

1. 同一账号开两个客户端，各自跑 4 个 subagent 任务。
2. 查 `~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表：
   期望每个客户端 3 个成功、1 个 `user concurrency limit exceeded`，合计 6 个成功。
3. 若合计只有 3 个成功，说明两个客户端的 device_mid 相同 —— 回到第 6 节第一条。
