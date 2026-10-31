# ZCode 多开：机制、实现与验证

> 版本：z-switch v1.6.0+（`feat/claim-activity-fallback` 分支）
> 结论先行：**不改 ZCode 的并发上限，让多个客户端各自拥有独立的 subagent 槽。**
> 1 个客户端 = 1 主 + 2 子 = 3 条执行流；N 个客户端 = N×3。

---

## 1. 底层机制（为什么多开有用）

**最终结论（2026-10-31 13:15 实测修正）**：

> 并发上限**按账号（user）记账**，每个账号 **3 个 subagent**。
> 服务端返回的错误是 `rate_limited / "user concurrency limit exceeded"`。
> **多开的价值 = 让多个不同账号同时跑**，不是让同一个号突破 3。

实测数据（两个不同账号、两个独立客户端、同时各发 4–5 个 subagent）：

| 时间 | 客户端 | 提交 | 成功 | 被拒 |
|---|---|---|---|---|
| 13:15:01 | A | 4 | **3** | 1 |
| 13:15:12 | B | 5 | **3** | 2 |

两边时间重叠、各自跑满 3 个、共 **6 个 subagent 并发成功**。第 4 个开始被拒。

**踩过的坑**：同一账号在两台机器/两个客户端上跑，会**共享**这 3 个名额。
案例：本地槽 1 用 `j2843467603`，而该号正在另一台电脑跑 → 本地被拒；
反过来默认实例的 `18231207670` 在别处活跃 → 它提交的 4 个全被拒、连主 agent 都 429。

> 登录不占名额，只有正在跑的请求才占。

已排除的维度：device_mid（两实例 mid 不同仍共享）、userData、出口 IP、实名、机器指纹。


## 2. 隔离手段（全部是官方环境变量）

| 变量 | 作用 |
|---|---|
| `ZCODE_DESKTOP_APPLICATION_NAME` | 改 Electron userData 默认目录 → 绕开单实例锁 |
| `ZCODE_DESKTOP_USER_DATA_DIR` | 显式指定 userData（Cookies / Local Storage 独立） |
| `ZCODE_DESKTOP_SESSION_DATA_DIR` | 会话数据目录 |
| `ZCODE_DESKTOP_HOME_DIR` | setting.json 分叉 |

**不重打包、不改安装目录、不改客户端上限。**

启动前必须清掉 `ELECTRON_RUN_AS_NODE`，否则 ZCode 会以 node 模式启动并立即退出。

## 3. 代码结构

```
src-tauri/src/slots.rs      新增：槽的定义、启动、按槽杀、账号指派、视图
src-tauri/src/lib.rs        注册 5 个命令：slots_list / slot_open / slot_assign / slot_kill / slot_remove
src/main.js                 切换气泡、置灰逻辑、多开入口
src/locales/{zh,en}.js      文案
src/styles.css              .sw-pop 气泡样式
```

### 数据

`~/.zcode-switch/slots.json`

```json
[{ "id": "1", "app_name": "ZCodeSlot1",
   "user_data_dir": "C:\\Users\\<你>\\.zcode-switch\\slots\\slot1",
   "account_id": "3e83a86b-...", "created_at": 1793418000 }]
```

槽的 userData 放在 `~/.zcode-switch/slots/slotN/`，不污染用户目录。

### 启动

```rust
Command::new(zcode_path)
    .arg(format!("--user-data-dir={}", slot.user_data_dir))
    .env_remove("ELECTRON_RUN_AS_NODE")
    .env("ZCODE_DESKTOP_APPLICATION_NAME", &slot.app_name)
    .env("ZCODE_DESKTOP_USER_DATA_DIR", &slot.user_data_dir)
    .env("ZCODE_DESKTOP_SESSION_DATA_DIR", format!("{}\\session", slot.user_data_dir))
```

`--user-data-dir` 同时让**按槽杀进程**成为可能：主进程命令行里带这个参数，用 PowerShell/CIM 匹配即可。

### 进程识别

```rust
fn is_main(cmd: &str) -> bool {
    !cmd.contains("--type=")      // Electron 子进程
        && !cmd.contains("zcode.cjs")  // node 子进程（app-server / plugin-host）
}
```

只认主进程，否则 `app-server`、`__zcode-plugin-host` 会被误判成客户端（实测 13 个进程里只有 1 个是主进程）。

### 指派账号

`credentials.json` 是全局共享的（`~/.zcode/v2/credentials.json`），ZCode 只在启动时读一次。所以：

```
写该账号凭据 → 若该槽在运行则杀掉 → 重启该槽
```

约 5–10 秒。槽运行起来后就不受 live 变化影响（已验证只在启动读一次）。

## 4. 交互设计

**账号行的「切换」按钮** → 弹气泡：

```
把「旅行者8095」登录到
┌──────────────────────────┐
│ 旅行者1636      默认客户端 │   ← 运行中的客户端，点击 = 把 8095 登录到它
├──────────────────────────┤
│ + 多开一个客户端           │   ← 常驻，用 8095 新建一个独立客户端
└──────────────────────────┘
```

- 气泡里列出**所有正在运行的客户端**，显示各自登录的账号；没登录的显示「未登录」
- 已被某个运行中客户端占用的账号，切换按钮置灰显示「已登录」，悬停提示占用者
- 「多开」常驻，与当前点的是哪个账号绑定

## 5. 已验证

| 项 | 结果 |
|---|---|
| 未登录槽能否正常启动 | ✅ 清空 credentials 后窗口正常，日志 `restoreCachedSession skipped: no active provider` |
| 已登录账号置灰 | ✅ 默认实例登录 1636 → 1636 行显示「已登录」 |
| 气泡列出运行中客户端 | ✅ 显示「旅行者1636 / 默认客户端」 |
| 多开项渲染 | ✅ 常驻显示 |
| 两个客户端共存 | ✅ `mains=2 slots=1`，槽 1 带 `--user-data-dir`，agent/billing 全部正常 |
| 槽识别 | ✅ 日志 `views: procs=26 mains=2 slots=1 accounts=4 live=Some("旅行者1636")` |
| 并发互不影响 | 见 §1（各 2 个 subagent，DOM `fail=2` 固定） |

## 6. device_mid 必须每客户端唯一（踩过的坑）

第一版实现漏了这条，现象是：两个客户端各只有一个主 agent + 一个子 agent，再要更多就都被拒。

原因：`~/.zcode/v2/telemetry-state.json` 只有一份，ZCode **只在启动时读一次**。
如果两个客户端的 userData 不同、但启动时那个文件里是同一个 mid，**服务端会把它们算成同一台设备**，
2 个 subagent 槽被两个客户端分掉——各 1 个，再加就拒。

现在的做法：

| 对象 | mid 来源 |
|---|---|
| 槽（绑定账号） | 该账号的 `virtual_device_mid` |
| 槽（未绑定账号） | 启动时生成 UUID，写回 `slots.json` 的 `device_mid` |
| 默认实例（「启动 ZCode」） | `~/.zcode-switch/default-device-mid.txt`，首次生成后固定 |

**默认实例用的是哪个账号，也要在启动时记下来**（`~/.zcode-switch/default-slot.json`）。
不能运行时读 `credentials.json` —— 之后别的槽启动会把 live 改写成它自己账号的凭据，
读 live 会把默认实例错认成那个槽的账号（实测踩过：默认实例被显示成「旅行者8095」）。

启动任何客户端前都会把对应的 mid 写进 `telemetry-state.json`，ZCode 起来读到的就是自己的。

**排查方法**：看 `~/.zcode-switch/slots.json` 里每个槽的 `device_mid` 是否互不相同，
且不等于默认实例的 `default-device-mid.txt`。

## 7. 注意

1. **凭据共享**：同一时刻只有一个 `credentials.json`。给槽 B 换号会改 live，重启槽 A 前 z-switch 会自动写回 A 记录的账号，但如果手动在 ZCode 里换号，z-switch 的记录会失真。
2. **同账号不能双开**：UI 已置灰防呆。两个槽抢同一份 token 没有意义。
3. **每槽约 1GB 内存**：32 GB 内存建议 3–4 个槽。
4. **`kill_zcode()`（关 ZCode 按钮）仍是全杀**：会关掉所有槽。按槽关请用气泡或后续加的槽管理。

## 7. 待验证

- 「同 mid 的两个实例是否共享槽」未做干净验证（之前那次被僵尸槽污染）。当前实现每槽写各自账号的 mid，实际等于不同 mid。
- 槽位冷却（`cooldownMs` / `idle_reset`）的定量恢复时间。
- `configs.dynamicWorkflow` 里并发字段的确切名称，以及是否存在更高档位下发 `limit > 2`。

## 8. 复现脚本

`tools/zcode-cdp/`

| 脚本 | 用途 |
|---|---|
| `cdp-probe.js <port> <expr>` | 在 renderer 里执行任意表达式 |
| `cdp-send.js <port> <text>` | 填入输入框并回车发送 |
| `cdp-concurrency.js <port> <task> <secs>` | 发任务并周期采样 subagent 状态 |
| `cdp-text.js <port> [kw] [tail]` | 读取会话文本 / 过滤关键字 |

前提：ZCode 以 `--remote-debugging-port=<port>` 启动。