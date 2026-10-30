# 活动领取机制说明

本文记录 z-switch 与官方「活动额度」（例：`ZCODE WEEKEND BUILD` 每天 5 亿
`GLM-5.3-Flash` token）之间的交互方式，以及本 fork 为什么要在 `billing/preview` 之外
再引入一层 `plan_id` 来源。

所有结论都来自本机抓到的真实响应，接口都在 `https://zcode.z.ai/api/v1/zcode-plan/billing/`。

## 1. 三个接口各干什么

| 接口 | 作用 | 关键点 |
|---|---|---|
| `GET /preview?app_version=&platform=` | 服务端「现在有什么可以领」 | 可能 `code=0` 但**没有 `data`** |
| `GET /balance?app_version=&platform=` | 该账号**已经拥有**的套餐与余额 | 必须有 `X-Device-Mid`，否则 `{"code":3001,"msg":"parameter error"}` |
| `POST /claim`，body `{"plan_id":"..."}` | 真正领取 | 只要 `plan_id` 合法就能成；**实测不强制验证码** |

### `preview` 会返回空

```json
{"code":0,"msg":"","logid":"20261030154929354a6d356aba37783f96"}
```

`code` 是 0，但没有 `data`。上游实现把这种响应当成「0 个可领套餐」，于是账号行里既不显示
活动，也没有领取入口——**即使服务端随后接受 `claim`**。

### `claim` 只认 `plan_id`

```json
POST /claim  {"plan_id":"zcode-v3-start-plan-1030-wk"}
→ 200 {"code":0,"message":"套餐领取成功",
       "data":{"plan":{"user_plan_id":"upl_2116195668875837440",
                       "plan_id":"zcode-v3-start-plan-1030-wk",
                       "status":"active",
                       "entitlements":[{"show_name":"GLM-5.3-Flash",
                                        "grant_units":500000000,
                                        "period":"one_time"}]}}}
```

这次请求**没有携带任何 `X-Aliyun-Captcha-Verify-*` 头**，同样成功。反过来说：

- `plan_id` 写错（例如少了批次后缀，`zcode-v3-start-plan`）→ `{"code":3001,"msg":"parameter error"}`
- 也就是说 `3001` 通常表示**参数不对**，而不是「缺验证码」

### `balance` 是「已领」的唯一可靠证据

```json
{"code":0,"data":{
  "plans":[{"plan_id":"zcode-v3-start-plan-1030-wk","status":"active","starts_at":1793375339,...}],
  "balances":[{"plan_id":"zcode-v3-start-plan-1030-wk",
               "total_units":500000000,"used_units":0,"remaining_units":500000000,
               "expires_at":1793581200}]}}
```

一个 `plan` 只有在**领取成功之后**才会出现在这里。所以：

> `balance.plans` 里存在某个 `plan_id` ⇔ 这个账号已经领过该活动。

这正是「不显示已领按钮」的判定依据。

## 2. 判定流程（本 fork 的实现）

```
preview
 ├─ 有 data.plans ──────────────→ 直接作为可领列表，写入活动池，返回
 └─ 空 / 出错
      ├─ ZSW_CLAIM_FROM_CACHE=0 ─→ 返回空（上游行为）
      └─ 继续
           balance 请求（失败重试 1 次，间隔 700ms）
            ├─ 失败 ─────────────→ 返回空（保守，不猜）
            └─ 成功
                 ├─ 把本次见过的 plan 写入活动池（即使本账号已领）
                 ├─ 候选 = 本账号缓存 ∪ 活动池
                 ├─ 剔除 balance 里已拥有的 plan_id（并剪掉本账号缓存中的对应项）
                 └─ 仍有剩余 → 返回（界面出现「领取」）；否则返回空
```

判定表：

| 情况 | 界面 |
|---|---|
| `preview` 返回了 `plan` | 显示「领取」 |
| `preview` 空，`balance` 已有该 `plan_id` | **不显示**（已领过） |
| `preview` 空，活动池里有、`balance` 没有 | 显示「领取」 |
| `balance` 查不到（超时/失败） | **不显示**（保守） |

## 3. 活动池与缓存文件

路径：`~/.zcode-switch/claim-plan-cache.json`

```json
{
  "__activity": {
    "saved_at": 1793375890,
    "history": [
      {
        "plan_id": "zcode-v3-start-plan-1030-wk",
        "name": "ZCODE WEEKEND BUILD",
        "first_seen": 1793371589,
        "last_seen": 1793375890,
        "plan": { "plan_id": "...", "name": "...", "grants": ["GLM-5.3-Flash · 5亿 Token（一次性）"], "...": "..." }
      }
    ]
  },
  "<account-id>": {
    "saved_at": 1793375890,
    "plans": [ "…该账号见过、但 balance 里还没有的 plan…" ]
  }
}
```

- `__activity.history`：活动池，与账号无关；`last_seen` 用于排序与 TTL，也是界面上
  「当前 / 上次」两行标识的来源
- `<account-id>.plans`：该账号自己的候选；领过之后会被剪掉
- 兼容旧格式：早期版本写的是 `__activity: {saved_at, plans: [...]}`，读取时会自动折算成
  `history`

TTL：活动池 `ACTIVITY_TTL_SECS = 36h`（用于「还能不能作为候选」），账号缓存
`PLAN_CACHE_TTL_SECS = 7d`。UI 显示历史不受 TTL 限制。

## 4. 日志

`%LOCALAPPDATA%\com.zswitch.app\logs\oauth.log`

```
23:22:49 [claim] preview-empty account=<id>
23:22:49 [claim] cache-skip    account=<id> reason=balance-unavailable
23:31:12 [claim] preview-cache account=<id> plans=1 owned=1
23:31:12 [claim] cache-none    account=<id> candidates=1 owned=1
```

| 事件 | 含义 |
|---|---|
| `preview` | `preview` 给了可领列表 |
| `preview-empty` | `preview` 返回了但没有 `data` |
| `preview-err` | `preview` 请求本身失败（常见 `os error 10060` 连接超时） |
| `preview-cache` | 用缓存/活动池兜底成功，界面会出现「领取」 |
| `cache-none` | 有候选，但全被 `balance` 判为已领（`candidates=1 owned=1`） |
| `cache-skip` | `balance` 不可用，保守不显示 |

## 5. 已知限制

1. **活动结束后仍可能短暂显示**：活动池 TTL 36 小时，跨日活动结束后如果还在窗口内，点
   「领取」会被服务端拒绝（返回活动不存在/已结束）。这是「宁可多显示一次」的取舍，换掉了
   「明明有活动却看不到」。
2. **`balance` 抖动会漏显示**：网络不好时不显示，而不是猜。日志里能看出是 `cache-skip`。
3. **验证码流程没简化**：`submit_claim` 仍要求非空 `X-Aliyun-Captcha-Verify-Param`，所以
   UI 上点「领取」还会弹 z-switch 自己的阿里云验证码窗。服务端实测不校验，这条路可以再省，
   但要改前端的等待逻辑，未在本次改动范围内。
4. **`plan_id` 复用是行为推断**：同一天所有账号用同一个 `plan_id` 是观察到的现象，不代表
   官方承诺；如果将来改成按账号发不同 `plan_id`，活动池依然能工作（只是每账号各一条）。
