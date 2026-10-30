# 变更记录

本仓库是 [pjpv/zcode-switch](https://github.com/pjpv/zcode-switch) 的 fork。
`main` 只用来跟踪上游，本 fork 的所有改动都落在 `feat/*` 分支上，便于随时 rebase。

---

## feat/activity-fallback-per-account-refresh · 基于上游 `v1.6.0`

### 修复：`billing/preview` 为空时被误判成「没有可领取」

**现象**：账号确实有官方活动额度可领（ZCode 客户端里能看到领取卡、能领成功），但 z-switch
的账号行里什么都不显示，也没有任何领取入口。

**根因**：`claim.rs` 里「有没有可领套餐」只认 `GET /api/v1/zcode-plan/billing/preview` 的
`data.plans`。这个接口在活动已发放但未领取的时候，可能返回

```json
{"code":0,"msg":"","logid":"..."}
```

——`code` 是 0，但**没有 `data`**。旧逻辑把它当成「0 个可领」，于是整条领取链路都走不到；
而与此同时 `POST /billing/claim` 带上正确的 `plan_id` 是能成功的（服务端并不要求
`billing/preview` 先返回过它）。

**改动**（`src-tauri/src/claim.rs`、`lib.rs`、`cli.rs`）：

1. **活动池（跨账号）**：`plan_id` 是**活动级**标识（例 `zcode-v3-start-plan-1030-wk`），
   同一活动对所有账号通用。把「这台机器见过的活动」记进
   `~/.zcode-switch/claim-plan-cache.json` 的 `__activity`，每条带 `first_seen` / `last_seen`；
   它既不随某个账号领完而消失，也不会被某账号的剪枝带走。新登录、还没领过的号因此也能
   看到这是什么活动。
2. **已领过滤**：`GET /billing/balance` 里出现过的 `plan_id` 代表「这个号已经拿到过」。
   这些 `plan_id` 会从候选里剔除，并从该账号的缓存中剪掉——**领过的号不会再有「领取」按钮**。
3. **候选来源顺序**：`preview` 有数据就直接用；`preview` 为空时，用「本账号缓存 ∪ 活动池」
   减去「`balance` 已拥有」，剩下的才显示。
4. **保守失败**：`balance` 请求失败（超时/网络）时**一律不显示**，宁可少显示一次，也不给
   一个点了会失败、或者本来就已领的按钮。
5. **诊断日志**：`%LOCALAPPDATA%\com.zswitch.app\logs\oauth.log` 会写
   `preview` / `preview-empty` / `preview-err` / `preview-cache` / `cache-none` / `cache-skip`，
   带 `candidates=`、`owned=` 计数，能直接看出是「没有活动」「活动被已领过滤」还是「网络失败」。
6. 环境变量开关：
   - `ZSW_CLAIM_FROM_CACHE=0`：退回上游原版行为（只看 `preview`）
   - `ZSW_NO_PLAN_CACHE=1`：不写缓存
   - 活动池有效期 `ACTIVITY_TTL_SECS`（默认 36 小时）；账号缓存 `PLAN_CACHE_TTL_SECS`（7 天）

### 新增：账号库标题右侧显示活动标识

`ACCOUNTS · 账号库` 右边两行小字，格式 `plan_id · 活动名`：

- 第一行：**最近一次**业务请求里见到的活动
- 第二行：**上一次**见过的活动（前缀 `上次` / `prev`）

用途是人工比对：官方是不是又发了新活动；或者明明知道有活动、这边却没出现（说明活动池
没拉到 / 被已领过滤 / 网络失败，配合日志就能定位）。

数据来源：新增 Tauri 命令 `claim_activities`，读活动池 `history`，按 `last_seen` 倒序。

### 合入：上游 PR #34（单账号「刷新资格」按钮）

[`pjpv/zcode-switch#34`](https://github.com/pjpv/zcode-switch/pull/34)
`feat(ui): 账号行内新增单账号「刷新资格」按钮`（作者 wxk66）。

原样应用，无冲突：账号行操作区最左新增 ↻ 按钮，点击只对该账号执行 `claim_refresh`
（激活活动资格 + 重查可领套餐），与全局刷新共用 60 秒冷却、与「全部领取 / 自动领取 /
手动领取」互斥。四个文件：`src/main.js`、`src/locales/{zh,en}.js`、`src/styles.css`。

### 构建注意（Windows）

从源码出可用二进制必须带 `custom-protocol` feature，否则 Tauri 会按 dev 模式把
`devUrl` 编进去，运行时窗口只剩 `ERR_CONNECTION_REFUSED`：

```powershell
cargo build --release --features tauri/custom-protocol
```

另外本机 `TEMP` 若是 8.3 短名（`C:\Users\ZHENGJ~1\...`），`link.exe` 会报
`LNK1104: 无法打开文件 lnk{...}.tmp`，把 `TMP`/`TEMP` 指到长路径即可。详见
[`docs/BUILD.md`](docs/BUILD.md)。
