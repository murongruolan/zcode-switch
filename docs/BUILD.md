# 开发与构建

面向要改代码或自己出二进制的人。运行环境、使用方式见 [README](../README.md)。

## 环境

| 依赖 | 版本/说明 |
|---|---|
| Node.js | 18+（只用于 Vite 构建与 Tauri CLI） |
| Rust | stable，`x86_64-pc-windows-msvc` |
| MSVC | Visual Studio 2022 Build Tools（`cl.exe` / `link.exe`） |
| Windows SDK | 10.0.22621 或更新 |
| WebView2 Runtime | 系统一般自带；Tauri 2 不做静态打包 |

不需要装完整 VS：`Build Tools` 勾选「使用 C++ 的桌面开发」即可。

## 构建

```powershell
npm install
npm run build                       # vite → dist/
cd src-tauri
cargo build --release --features tauri/custom-protocol
```

产物：`src-tauri/target/release/zcode-switch.exe`（单文件，约 13–14 MB）。

也可以用官方 CLI，它会自己带 feature 和 `beforeBuildCommand`：

```powershell
npx tauri build --no-bundle
```

> 注意：本机若 `node` 实际指向 Electron/宿主程序，`npx tauri` 会把宿主 exe 路径当成子命令，
> 报 `unrecognized subcommand '...\App.exe'`。换成系统 Node（`C:\Program Files\nodejs\node.exe`）
> 或直接用上面的 `cargo build --release --features ...`。

## 两个必须知道的坑

### 1. 少了 `custom-protocol` 会编出「连 127.0.0.1:5173」的二进制

`tauri/build.rs` 里的逻辑是：

```rust
let dev = !has_feature("custom-protocol");
```

也就是说 **release 也要显式启用该 feature**，否则 `generate_context!` 会把 `devUrl`
（`http://127.0.0.1:5173`）当成前端入口嵌进去。表现是：窗口能开，但页面是
`ERR_CONNECTION_REFUSED`，且 exe 里能搜到 `127.0.0.1:5173` 而搜不到 `dist/assets` 的产物名。

自查：

```powershell
findstr /M /C:"127.0.0.1:5173" src-tauri\target\release\zcode-switch.exe
# 命中也正常（配置里本来就有 devUrl 字符串）；关键看是否嵌入了 dist 资产：
findstr /M /C:"main-" src-tauri\target\release\zcode-switch.exe
```

### 2. 8.3 短名 `TEMP` 会让链接失败

```
LINK : fatal error LNK1104: 无法打开文件“C:\Users\ZHENGJ~1\AppData\Local\Temp\lnk{...}.tmp”
```

`link.exe` 在临时目录创建文件失败。把 `TMP` / `TEMP` 指到长路径即可：

```powershell
$env:TMP = "$PWD\..\build-tmp"; $env:TEMP = $env:TMP
cargo build --release --features tauri/custom-protocol
```

## 目录

```
src/                前端（无框架，原生 ES 模块 + Vite）
  main.js           主窗逻辑（账号行、领取、刷新）
  locales/          zh / en 文案表（scripts/check-i18n-keys.mjs 校验键一致）
  styles.css        全部样式
  captcha.js / settings.js / ui.js / icons.js
src-tauri/src/
  lib.rs            Tauri 命令注册与窗口/托盘
  store.rs          账号库、切换、设备身份
  claim.rs          活动领取（preview / balance / claim 与活动池）
  quota.rs          额度查询与渠道选择
  oauth.rs          工具内登录
  cli.rs            --cli 子命令
  cipher.rs/zcrypto.rs  凭据加解密
docs/
  CLAIM-ACTIVITY.md 活动领取机制与判定表（改 claim 前先读这个）
  BUILD.md          本文件
```

## 本地自检

```powershell
npm run check:i18n      # zh/en 键是否齐、静态调用点是否都有键
node scripts/check-i18n-keys.mjs
.\src-tauri\target\release\zcode-switch.exe --cli list
.\src-tauri\target\release\zcode-switch.exe --cli claim-preview --id <账号id>
```

`claim-preview` 是最快的回归入口：它走的正是界面用的那条链路（活动池 + 已领过滤）。

## 调试领取逻辑

1. 看日志：`%LOCALAPPDATA%\com.zswitch.app\logs\oauth.log`，事件名与含义见
   [CLAIM-ACTIVITY.md](CLAIM-ACTIVITY.md#4-日志)
2. 想临时回到上游行为：启动前设 `ZSW_CLAIM_FROM_CACHE=0`
3. 想清掉活动池重来：删 `~/.zcode-switch/claim-plan-cache.json`（不影响账号库）

## 发布

`.github/workflows/release.yml` 是上游的三个平台构建流程，本 fork 没有改。自己出包时注意
上面的 `custom-protocol`：上游 workflow 走的是 `tauri build`，本来就带上了，但手写
`cargo build` 容易漏。
