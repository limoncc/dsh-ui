# dsh-ui

用 Tauri 2 封装 [DeepSeek Harness](https://github.com/deepseek-ai) Web UI 的 macOS 桌面客户端。

壳本身极简（无前端框架）：Rust 侧启动系统安装的 `dsh` 后端（`dsh web --no-open --port 0`），
从 stdout 解析就绪信号拿到带 launch token 的地址后，由内嵌 WebView 加载。

## 前提

- macOS
- Node.js ≥ 22（`dsh` 依赖 `node:sqlite`）
- DeepSeek Harness CLI：`npm i -g @deepseek-ai/dsh`

数据与 CLI 共享 `~/.dsh`（会话互通）。

## 功能

- 自动拉起/重启 `dsh` 后端；崩溃后可一键重启
- **原生标题栏按钮**（红绿灯右侧）：`🟢 状态`（点击重启 dsh）、日志、设置
- **设置窗口**：自定义 dsh / node 路径（留空 = 自动探测），支持即时测试与保存后自动重启
  （持久化在 `~/Library/Application Support/com.dsh.ui/config.json`）
- **安装指引**：缺少 Node/dsh 或版本过低时，按错误类型给出分步安装命令（带复制按钮）
- 明暗主题跟随：dsh 页面切换主题时窗口原生外观与壳页面同步（导航信号通道，无 IPC 依赖）
- 原生菜单：编辑、缩放（Cmd +=/-/0，25%~500%）
- 系统托盘（显示/退出）、关窗隐藏到托盘、窗口位置记忆、单实例
- 导航锁定：仅允许当前 dsh 端口与壳页面，外链转系统浏览器
- 日志落盘 `~/Library/Logs/dsh-ui/dsh.log`，错误页附 stderr 尾部
- 防孤儿守护：APP 无论何种方式退出（含被强杀），dsh 均被自动清理

## 开发

```sh
npm install
npm run dev      # tauri dev
```

## 打包

```sh
npm run build    # 产出 .app 与 .dmg（src-tauri/target/release/bundle/）
```

## 测试

```sh
cd src-tauri && cargo test   # 单元/集成测试（配置、探测、进程生命周期、stdin 守护等）
```

## 结构

```
ui/                 壳页面（loading / settings，零构建）
src-tauri/src/
  dsh.rs            环境探测、就绪行解析、进程状态机、日志（核心）
  settings.rs       应用配置（dsh/node 路径自定义）
  mac_titlebar.rs   macOS 原生标题栏按钮（状态/日志/设置/终端）
  lib.rs            窗口/菜单/托盘/布局装配与 Tauri 命令
```
