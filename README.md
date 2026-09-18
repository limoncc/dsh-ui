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

- 自动拉起/重启 `dsh` 后端；崩溃后底部状态条可一键重启
- 底部常驻状态条：连接状态、重启、打开日志目录、**在窗口下方打开系统终端**
- 明暗主题跟随：dsh 页面切换主题时窗口原生外观同步
- 原生菜单：编辑、缩放（Cmd +=/-/0，25%~500%）
- 系统托盘（显示/退出）、关窗隐藏到托盘、窗口位置记忆、单实例
- 导航锁定：仅允许当前 dsh 端口与壳页面，外链转系统浏览器
- 日志落盘 `~/Library/Logs/dsh-ui/dsh.log`，错误页附 stderr 尾部

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
cd src-tauri && cargo test   # 47 个单元/集成测试（含假 dsh 进程生命周期测试）
```

## 结构

```
ui/                 壳页面（loading / bar，零依赖，经 window.__TAURI__ 通信）
src-tauri/src/
  dsh.rs            环境探测、就绪行解析、进程状态机、日志（核心，全量单测）
  lib.rs            窗口/菜单/托盘/主题装配与 Tauri 命令
```
