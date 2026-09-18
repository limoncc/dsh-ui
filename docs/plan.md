# dsh-ui：用 Tauri 封装 DeepSeek Harness Web 为 macOS APP

## Context（背景与动机）

把 `/Users/xiaobai/dev/deepseek-harness/`（DeepSeek agent harness：Node 后端 + React/Vite 前端）的 Web UI 封装成桌面 APP，项目落点为空目录 `/Users/xiaobai/dev/dsh-ui/`。用户已有可用的 Tauri 2 参考项目 `/Users/xiaobai/dev/deepseek_app/`（封装 chat.deepseek.com 的浏览器壳），其托盘、主题跟随、原生菜单等模式直接移植。

**架构结论**：dsh 后端必须有 Node.js ≥22（用户已确认用系统 Node + `npm i -g @deepseek-ai/dsh`）；前端必须由 dsh 后端托管（`window.__DSH_BOOT__` 注入）且核心通信走 WebSocket/SSE → **WebView 加载 loopback URL，不做自定义协议，不把 dist 打进 APP**。壳本身极简（无前端框架），复用 deepseek_app 的"原生壳"全套模式。

### 已验证的关键事实

- `dsh web --no-open --port 0`：就绪后 stdout 打印 `dsh web: http://127.0.0.1:<port>/?token=<token>`（`packages/bundle/web-app/src/index.ts:252-297` announceReady，官方 supervisor 就绪信号；行尾可能带 ` (LAN: ...)` 仅 0.0.0.0 绑定时出现）
- token 每进程随机（`browser-auth.ts:52-58`）→ **无法复用外部已运行的 dsh 实例**，APP 必须自己 spawn；带 token URL 首次 GET / 换 30 天 HttpOnly cookie（cookie 密钥持久化于 `~/.dsh/.credentials.yaml`，跨 dsh 重启有效）
- `--port 0` 由 OS 选空闲端口（源码 startup.ts 支持并明确文档化）；`--host 0.0.0.0` 被拒绝，仅 127.0.0.1
- Tauri 2 remote capabilities 的 URL 匹配是 URLPattern，`http://127.0.0.1:*` 合法（tauri-utils `acl/mod.rs:280-317`）
- 多 WebView 需 tauri `unstable` feature；子 webview 需 `.auto_resize()` 否则不随窗口缩放（`webview/mod.rs:1036`）；`Webview::navigate/set_zoom`、`Window::set_effects`、`on_navigation` 均存在（tauri 2.11 已核实）
- `tauri-plugin-single-instance` 2.x 支持 macOS（docs.rs 平台表），必须最先注册
- **GUI PATH 陷阱**：Finder 启动的 APP 不继承 shell PATH；`/usr/local/bin/dsh` shebang 为 `#!/usr/bin/env node` → Rust 侧必须自行解析 `dsh`/`node` 绝对路径并构造子进程 PATH
- WKWebView 加载 `http://127.0.0.1` + ws:// loopback 无 ATS 障碍（如遇问题再给 Info.plist 加 `NSAllowsLocalNetworking`）
- 用户机器已有 `~/.dsh`（profiles/sessions/settings.yaml/storages），与 CLI 共享数据是特性

### 用户确认的需求

1. Node 来源：系统 Node ≥22 + 全局 dsh
2. 壳体验：托盘 + 关窗隐藏、明暗主题跟随、原生菜单 + 缩放、窗口状态记忆 + 单实例
3. 底部终端按钮：APP 窗口底部常驻一个按钮条，点击打开 Terminal.app 且窗口定位在 APP 正下方
4. 仅 macOS（bundle: app + dmg）

---

## 一、项目结构

```
dsh-ui/
├── package.json                  # 仅 devDep @tauri-apps/cli ^2；scripts: dev/build
├── ui/                           # frontendDist（静态，无框架）
│   ├── index.html                # 占位壳（满足 frontendDist，永不展示——窗口由 Rust 动态装配）
│   ├── loading.html              # 启动/错误页（状态点 + 错误信息 + 重试按钮）
│   └── bar.html                  # 底部条（状态 + 打开终端/重启/打开日志按钮）
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── icons/                    # 从 deepseek_app 复制图标组
│   └── src/
│       ├── main.rs               # 薄入口 → dsh_ui::run()
│       ├── lib.rs                # 装配：插件/菜单/托盘/窗口/webview/主题/单实例
│       └── dsh.rs                # dsh 子进程生命周期模块（核心）
```

**依赖清单**：

- `Cargo.toml`：`tauri = { version = "2", features = ["tray-icon", "unstable"] }`、`tauri-build = "2"`、插件 `tauri-plugin-shell`、`tauri-plugin-window-state`、`tauri-plugin-single-instance`、`serde`/`serde_json`、`libc`、macOS 专属 `objc2 0.6` + `objc2-app-kit 0.3`（NSAppearance，照抄 deepseek_app）、`image 0.25`（托盘图标）
- 不需要 `tauri-plugin-global-shortcut`（deepseek_app 有，dsh-ui 无此需求）

**tauri.conf.json 要点**：`productName: "dsh-ui"`、`identifier: "com.dsh.ui"`、`build.frontendDist: "../ui"`（无 devUrl/beforeDevCommand）、`csp: null`、`app.withGlobalTauri: true`（loading/bar 用 `window.__TAURI__.core.invoke`，零 npm 依赖）、`bundle.targets: ["app", "dmg"]`、窗口不在此定义（Rust 侧 `WindowBuilder` 装配多 webview；window-state 插件自动恢复尺寸位置）

**capabilities/default.json**（照抄 deepseek_app 结构改 remote）：

```json
{
  "identifier": "default",
  "windows": ["main"],
  "remote": { "urls": ["http://127.0.0.1:*"] },
  "permissions": [
    "core:default",
    "core:event:default",
    "core:window:allow-show", "core:window:allow-hide", "core:window:allow-focus",
    "core:webview:allow-set-webview-zoom",
    "core:window:allow-set-effects"
  ]
}
```

remote 段使 content webview（loopback 页面）能 invoke `report_theme`；loading/bar 是本地页面不受 remote 限制。

## 二、dsh 子进程生命周期（src/dsh.rs，核心模块）

状态机：`starting → ready → stopped | error(restarting)`，每次变迁 `emit("dsh://state", {status, port, error})`；`get_dsh_state` 命令供页面拉取当前态。

1. **环境检测**（spawn 前）：
   - 按序解析 `dsh`：继承的 `PATH` → `/usr/local/bin/dsh` → `/opt/homebrew/bin/dsh`；找不到 → error 态，错误页显示安装指引（`npm i -g @deepseek-ai/dsh`）
   - 解析 `node`（同序）并跑 `node --version`，major < 22 → error 态提示升级 Node
   - 子进程 `PATH` = `dirname(dsh) + ":" + dirname(node) + ":" + 继承 PATH`
2. **spawn**：`dsh web --no-open --port 0`，`process_group(0)`（unix CommandExt，便于整组杀）、`stdin(null)`、stdout/stderr pipe；`DSH_HOME` 透传（默认 `~/.dsh`）
3. **就绪解析**：BufReader 逐行读 stdout，匹配前缀 `dsh web: `，取其后第一个空白分隔 token，校验 `http://127.0.0.1:<port>/?token=` 形态（防误匹配 `dsh web: opening...` 散文行）；30s 超时（env `DSH_UI_BOOT_TIMEOUT` 可覆盖）→ error 态
4. **加载**：`get_webview("content").navigate(url)`（token URL，一次换取 cookie）
5. **崩溃处理**：子进程意外退出 → `stopped` 态；bar 显示"重启"按钮 → `restart_dsh` 命令重跑 spawn 流程（新进程新 token，重新 navigate）
6. **退出清理**：`RunEvent::ExitRequested` 中 kill：`libc::kill(-pid, SIGTERM)` → 轮询 `try_wait` 3s → `SIGKILL`；日志全量落盘 `~/Library/Logs/dsh-ui/dsh.log`（stderr 环形缓冲尾部 ~8KB 用于错误页展示；`open_log_dir` 命令在 Finder reveal）
7. **端口策略**：固定 `--port 0`——避免与用户手动 `dsh web` 或其他实例冲突；单实例锁保证 APP 自身不重复

## 三、窗口装配与壳功能（src/lib.rs）

**窗口**：`WindowBuilder("main")` 1200x800、min 640x480、`resizable`、macOS `TitleBarStyle::Transparent` + 隐藏标题栏（照抄 deepseek_app）；内部两个 webview（`unstable` 多 webview）：

- `content`：初始加载本地 `loading.html`，就绪后 navigate 到 dsh URL；`initialization_script` 注入 deepseek_app 的 THEME_DETECT_SCRIPT（MutationObserver 检测网页明暗 → `invoke('report_theme', theme)`）；`on_navigation` 锁定——仅放行 `http://127.0.0.1`，其余 URL 阻止并转系统浏览器打开（shell/opener）
- `bar`（高 36，贴底）：加载 `bar.html`，两个 webview 均 `.auto_resize()` 随窗口缩放；bar 显示连接状态点（listen `dsh://state`）+ 按钮：**打开终端**、重启（仅 stopped/error 时）、打开日志；listen `theme-changed` 切换亮暗 CSS

**命令**：`report_theme(theme)`、`open_terminal()`、`open_log_dir()`、`restart_dsh()`、`get_dsh_state()`

**移植 deepseek_app lib.rs 的功能**（逐项）：

| 功能 | 来源（deepseek_app/src/lib.rs） | 适配点 |
|---|---|---|
| 主题检测脚本 + report_theme + NSAppearance 跟随 | THEME_DETECT_SCRIPT / objc2-app-kit | 原样移植，theme-changed 事件额外驱动 bar/loading 配色 |
| 原生菜单 App/Edit/View/Window | menu 构建 | View 缩放目标改为 `get_webview("content").set_zoom()`（25%~500% 预设 + Cmd+=/-/0）；App 菜单加"打开终端 ⌘T" |
| 托盘 Show/Quit + 左键唤起 | tray | 原样 |
| 关窗隐藏（CloseRequested → hide + prevent_close） | window event | 原样 |
| Dock Reopen 恢复窗口 | RunEvent::Reopen | 原样 |
| window-state 插件 | 插件 | 原样（自动恢复窗口尺寸位置） |

**新增：单实例**：`tauri-plugin-single-instance`，在 builder 最先 `.plugin()` 注册；回调中 show + focus 主窗口（deepseek_app 无此功能，新写）

## 四、底部终端按钮（open_terminal）

1. Rust 取主窗口 `outer_position()` + `outer_size()`（物理像素）+ `current_monitor()` 尺寸
2. 计算终端 bounds：x = 窗口 x，y = 窗口 y + 窗口高 + 8；宽 = 窗口宽；高 = min(480, 屏幕底部余量)，clamp 不越屏
3. AppleScript（`/usr/bin/osascript`）：

```applescript
tell application "Terminal"
  activate
  do script ""
  set bounds of front window to {X, Y, X + W, Y + H}
end tell
```

按钮是 bar.html 里的常驻按钮（`invoke('open_terminal')`），菜单"打开终端 ⌘T"调同一命令。

## 五、开发流程：TDD（小功能 → 测试 → 通过 → git commit）

**可测性设计前置**：`dsh.rs` 中所有逻辑抽成可注入的纯函数/小结构——路径解析、版本比较、就绪行解析、导航放行判定、缩放档位、终端 bounds 计算均为纯函数直接单测；`DshProcess` 接受 `DshConfig { program, args, boot_timeout }` 注入，集成测试用 `sh -c` 假 dsh 脚本替代真实 dsh。测试运行 `cargo test`（src-tauri 内 `#[cfg(test)]`，无额外测试框架依赖）。

每个循环 = **先写失败测试 → 最小实现 → `cargo test` 全绿 → `git commit`**；GUI 行为（无法单测的部分）以 `npm run tauri dev` 人工冒烟作为该循环的"测试通过"标准，冒烟确认后才 commit。

| # | 循环 | 先写的测试 | commit 信息 |
|---|---|---|---|
| 0 | 脚手架 | 冒烟：`cargo check` + `tauri dev` 空窗口可起 | `chore: tauri 脚手架与项目配置` |
| 1 | 环境检测 | `resolve_dsh_path`（PATH 命中/候选目录命中/全未命中）、`parse_node_version`（v22.3.0→22、v18→拒、畸形输入）、子进程 PATH 构造 | `feat: dsh 与 node 环境检测` |
| 2 | 就绪行解析 | `parse_ready_line`：标准行→URL；带 `(LAN: …)` 后缀→只取首 token；`opening the default browser` 散文行→None；非 127.0.0.1/非 http→None；普通日志→None | `feat: dsh stdout 就绪行解析` |
| 3 | 进程状态机 | 假 dsh（`sh -c` 打印就绪行后 sleep）：到 ready 态并取得 URL；只打废话→超时（注入 100ms）→error；立即退出→stopped；退出清理 SIGTERM 全组终止 | `feat: dsh 子进程状态机与生命周期` |
| 4 | 窗口装配 | 冒烟：dev 起窗、loading→(假 dsh)→内容页 | `feat: 双 webview 窗口与 loading 装配` |
| 5 | 导航锁定 | `is_allowed_navigation(url, port)`：本机 dsh 端口→放行；其他 host/端口→拒（转系统浏览器） | `feat: webview 导航锁定` |
| 6 | 主题跟随 | 主题字符串归一化纯函数；冒烟：dsh UI 切深色→窗口与页面跟随 | `feat: 明暗主题跟随` |
| 7 | 菜单+缩放 | 缩放档位数组与 clamp（25%~500%、Cmd+=/-/0 边界）；冒烟：快捷键生效 | `feat: 原生菜单与缩放` |
| 8 | 托盘+关窗隐藏+单实例 | 冒烟：关窗隐藏、托盘唤起/退出、二次启动聚焦；冒烟即测试 | `feat: 托盘、关窗隐藏与单实例` |
| 9 | bar 状态接线 | 状态→文案/按钮可见性映射单测（Rust 侧或极薄 JS）；冒烟：假 dsh 崩溃→stopped→重启按钮→恢复 | `feat: 底部条状态与重启` |
| 10 | 终端按钮 | `terminal_bounds` 纯函数（窗口正下方、clamp 不越屏）；冒烟：Terminal.app 出现在 APP 下方 | `feat: 打开系统终端` |
| 11 | 打包 | 冒烟：`tauri build` 产出 .app/.dmg，双击（脱离终端 PATH）正常启动 | `build: macOS 打包配置` |

**Git 规范**：dsh-ui 先 `git init`（循环 0 内，首个 commit 建立在脚手架可构建之上）；每个循环一个 commit，测试不过不提交；commit 信息不写 `Co-Authored-By:` 行。

## 六、验证方案

**每个 TDD 循环的回归门**：`cargo test`（单测 + 假 dsh 集成测试）全绿才允许 commit；GUI 冒烟按第五节各循环标注执行。

端到端验收：

前提：本机已装 Node ≥22 与全局 dsh（已满足，`~/.dsh` 存在）。

1. `npm run tauri dev`：APP 启动 → loading 页 → 数秒内自动进入 dsh UI（wkwebview 内 WebSocket/SSE 正常，能新建会话对话）
2. `lsof -p <dsh pid>` 确认 dsh 监听随机端口；`ps` 确认属 APP 子进程
3. 终端按钮 → Terminal.app 新窗口出现在 APP 正下方
4. 主题：dsh UI 切深色 → 窗口原生外观与 bar 跟随变暗
5. 关窗 → 隐藏到托盘；托盘 Quit → APP 与 dsh 进程全部退出（`pgrep dsh` 为空）
6. 故障注入：临时改 PATH 探测路径（重命名探测）→ 显示 Node/dsh 安装指引；`kill -9 <dsh pid>` → bar 变 stopped + 重启按钮可用，点击恢复
7. 再次启动 APP → 聚焦已有窗口；窗口尺寸位置被记忆
8. `npm run tauri build` 产出 .app/.dmg，双击验证脱离终端环境（GUI PATH）仍可启动

## 七、风险与备选

- **多 webview `unstable` feature**：官方 feature 但标注不稳定；备选方案 B = 单 WebviewWindow + initialization_script 注入底部 DOM 条（dsh.rs/命令逻辑不变）
- **WKWebView ATS**：loopback http 理论豁免；若 ws:// 被拦，bundle Info.plist 加 `NSAllowsLocalNetworking`
- **dsh CLI 启动参数演变**：stdout 就绪行格式变化会破坏解析 → 解析函数集中一处 + 单测
