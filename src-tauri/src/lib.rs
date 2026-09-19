pub mod dsh;
pub mod pty;
pub mod settings;

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::webview::WebviewBuilder;
use tauri::window::WindowBuilder;
use tauri::{Emitter, Listener, Manager, State, WebviewUrl};

/// 底部状态条高度（逻辑像素）。
const BAR_HEIGHT: f64 = 36.0;

/// 内嵌终端面板高度（逻辑像素）。
const TERMINAL_HEIGHT: f64 = 320.0;

/// 注入 dsh 页面：检测网页明暗主题并回报给壳（移植自 deepseek_app）。
const THEME_DETECT_SCRIPT: &str = r#"
(function(){
    function getTheme(){
        var el;
        el=document.documentElement;
        if(el){
            if(el.classList.contains('dark'))return'dark';
            var a=el.getAttribute('data-theme');
            if(a==='dark')return'dark';
            a=el.getAttribute('data-color-scheme');
            if(a==='dark')return'dark';
            a=el.getAttribute('data-mode');
            if(a==='dark')return'dark';
            if(el.hasAttribute('dark'))return'dark';
        }
        el=document.body;
        if(el){
            for(var i=0;i<el.classList.length;i++){
                var c=el.classList[i];
                if(c==='dark')return'dark';
                if(c==='light')return'light';
            }
        }
        return'light';
    }
    function report(t){
        try{window.__TAURI_INTERNALS__.invoke('report_theme',{theme:t}).catch(function(){})}catch(e){}
        try{window.__TAURI_INTERNALS__.emit('theme-changed',{theme:t})}catch(e){}
    }
    function setup(){
        report(getTheme());
        var cb=function(){report(getTheme())},opts={attributes:true,attributeFilter:['class','data-theme','data-color-scheme','data-mode','style'],subtree:false};
        [document.documentElement,document.body].filter(Boolean).forEach(function(n){
            new MutationObserver(cb).observe(n,opts);
        });
    }
    if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',setup);
    else setup();
    var n=0,i=setInterval(function(){report(getTheme());if(++n>=20)clearInterval(i)},500);
})();
"#;

/// dsh 启动就绪的最长等待时间；可用 `DSH_UI_BOOT_TIMEOUT` 环境变量覆盖（秒）。
const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 30;

/// 壳对前端暴露的 dsh 运行状态。
#[derive(Clone, Serialize, Default)]
struct DshState {
    status: String, // "starting" | "ready" | "stopped" | "error"
    url: Option<String>,
    error: Option<String>,
    /// 错误分类，驱动前端渲染对应的安装指引；
    /// missing_dsh | missing_node | node_too_old | boot_timeout | crashed | spawn_failed
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
}

#[cfg(test)]
mod dsh_state_tests {
    use super::DshState;

    #[test]
    fn dsh_state_serializes_error_kind_for_frontend() {
        let state = DshState {
            status: "error".to_string(),
            url: None,
            error: Some("未找到 dsh".to_string()),
            error_kind: Some("missing_dsh".to_string()),
        };
        let json = serde_json::to_value(&state).expect("serialize");
        assert_eq!(json["status"], "error");
        assert_eq!(json["error_kind"], "missing_dsh");
        // 就绪状态不携带 error_kind 字段。
        let ready = DshState::default();
        let ready_json = serde_json::to_value(&ready).expect("serialize");
        assert!(ready_json.get("error_kind").is_none());
    }
}

struct AppState {
    process: Mutex<Option<dsh::DshProcess>>,
    state: Mutex<DshState>,
    /// dsh 就绪后记录的实际端口，用于导航放行判定。
    dsh_port: Mutex<Option<u16>>,
    /// content webview 当前缩放（百分比）。
    zoom: AtomicU32,
    /// 内嵌终端面板是否展开。
    terminal_open: AtomicBool,
    /// 内嵌终端的 PTY 会话（首次展开时创建）。
    pty: Mutex<Option<pty::PtySession>>,
}

/// 对 content webview 应用缩放。
fn apply_zoom(app: &tauri::AppHandle, percent: u32) {
    if let Some(content) = app.get_webview("content") {
        let _ = content.set_zoom(f64::from(percent) / 100.0);
    }
}

/// 构建 macOS 原生菜单（App/Edit/View/Window，移植自 deepseek_app）。
#[cfg(target_os = "macos")]
fn build_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let zoom_in = MenuItem::with_id(app, "zoom_in", "放大", true, Some("CmdOrCtrl+="))?;
    let zoom_out = MenuItem::with_id(app, "zoom_out", "缩小", true, Some("CmdOrCtrl+-"))?;
    let zoom_reset =
        MenuItem::with_id(app, "zoom_reset", "实际大小", true, Some("CmdOrCtrl+0"))?;

    let edit_menu = Submenu::with_items(
        app,
        "编辑",
        true,
        &[
            &PredefinedMenuItem::undo(app, None::<&str>)?,
            &PredefinedMenuItem::redo(app, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None::<&str>)?,
            &PredefinedMenuItem::copy(app, None::<&str>)?,
            &PredefinedMenuItem::paste(app, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::select_all(app, None::<&str>)?,
        ],
    )?;

    let view_menu = Submenu::with_items(
        app,
        "显示",
        true,
        &[
            &zoom_in,
            &zoom_out,
            &PredefinedMenuItem::separator(app)?,
            &zoom_reset,
        ],
    )?;

    let app_menu = Submenu::with_items(
        app,
        "dsh-ui",
        true,
        &[
            &PredefinedMenuItem::about(app, Some("关于 dsh-ui"), None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None::<&str>)?,
            &PredefinedMenuItem::hide_others(app, None::<&str>)?,
            &PredefinedMenuItem::show_all(app, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None::<&str>)?,
        ],
    )?;

    let window_menu = Submenu::with_items(
        app,
        "窗口",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None::<&str>)?,
            &PredefinedMenuItem::fullscreen(app, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None::<&str>)?,
            &PredefinedMenuItem::bring_all_to_front(app, None::<&str>)?,
        ],
    )?;

    app.set_menu(Menu::with_items(app, &[&app_menu, &edit_menu, &view_menu, &window_menu])?)?;
    Ok(())
}

#[tauri::command]
fn get_dsh_state(state: State<'_, AppState>) -> DshState {
    state.state.lock().unwrap().clone()
}

/// 诊断命令：壳页面把 JS 侧 API 状态回传，Rust 打到 stderr（dev 日志可见）。
#[tauri::command]
fn diag_report(source: String, info: String) {
    eprintln!("[diag] {source}: {info}");
}

/// Tauri 命令：重启 dsh（停止旧进程后重新探测并 spawn）。
#[tauri::command]
fn restart_dsh(app: tauri::AppHandle) {
    if let Some(process) = app.state::<AppState>().process.lock().unwrap().take() {
        process.stop();
    }
    start_dsh(&app);
}

/// Tauri 命令：在 Finder 中显示 dsh 日志目录。
#[tauri::command]
fn open_log_dir() {
    if let Some(dir) = dsh::log_dir() {
        let _ = std::process::Command::new("open").arg("-R").arg(dir).spawn();
    }
}

/// 首次展开终端时创建 PTY 会话，输出 base64 后经事件推给前端。
fn spawn_pty(app: &tauri::AppHandle) -> Result<(), String> {
    if app.state::<AppState>().pty.lock().unwrap().is_some() {
        return Ok(());
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let output_handle = app.clone();
    let exit_handle = app.clone();
    let session = pty::PtySession::spawn(
        pty::PtyConfig {
            shell: shell.into(),
            args: vec![],
            cwd: cwd.into(),
            rows: 24,
            cols: 80,
        },
        Arc::new(move |bytes| {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let _ = output_handle.emit("pty://output", encoded);
        }),
        Box::new(move |_code| {
            let _ = exit_handle.emit("pty://exit", ());
        }),
    )
    .map_err(|error| error.to_string())?;
    *app.state::<AppState>().pty.lock().unwrap() = Some(session);
    Ok(())
}

/// Tauri 命令：开合内嵌终端面板，返回开合后的状态。
#[tauri::command]
fn terminal_toggle(app: tauri::AppHandle) -> Result<bool, String> {
    let state = app.state::<AppState>();
    let open = !state.terminal_open.load(Ordering::Relaxed);
    state.terminal_open.store(open, Ordering::Relaxed);
    if open {
        spawn_pty(&app)?;
    }
    relayout(&app);
    Ok(open)
}

/// Tauri 命令：向内嵌终端写入键盘输入。
#[tauri::command]
fn pty_write(state: tauri::State<'_, AppState>, data: String) -> Result<(), String> {
    let guard = state.pty.lock().unwrap();
    match guard.as_ref() {
        Some(session) => session.write(data.as_bytes()).map_err(|error| error.to_string()),
        None => Ok(()), // 会话尚未建立或已退出：静默忽略
    }
}

/// Tauri 命令：同步终端尺寸（前端 fit 后调用）。
#[tauri::command]
fn pty_resize(state: tauri::State<'_, AppState>, rows: u16, cols: u16) -> Result<(), String> {
    let guard = state.pty.lock().unwrap();
    match guard.as_ref() {
        Some(session) => session.resize(rows, cols).map_err(|error| error.to_string()),
        None => Ok(()),
    }
}

/// Tauri 命令：dsh 页面的主题检测脚本经此回报主题变化。
#[tauri::command]
fn report_theme(app: tauri::AppHandle, theme: String) {
    let normalized = dsh::normalize_theme(&theme).to_string();
    let handle = app.clone();
    let callback = handle.clone();
    let _ = handle.run_on_main_thread(move || apply_window_theme(&callback, &normalized));
}

/// 让 macOS 窗口原生外观（材质与 NSAppearance）跟随 dsh 页面主题，
/// 并广播给 bar/loading 页面切换配色。必须在主线程调用。
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn apply_window_theme(app: &tauri::AppHandle, theme: &str) {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSApp, NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
        use tauri::window::{Color, Effect, EffectState, EffectsBuilder};

        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let dark = theme == "dark";
        if let Some(window) = app.get_window("main") {
            let effects = EffectsBuilder::new()
                .effect(if dark { Effect::Sidebar } else { Effect::ContentBackground })
                .state(EffectState::Active)
                .radius(0.0)
                .color(if dark { Color(0, 0, 0, 255) } else { Color(255, 255, 255, 255) })
                .build();
            let _ = window.set_effects(effects);
        }
        // objc2 新版把 extern static 与部分 FFI 声明标为 safe：仅 static 访问需要 unsafe。
        let name = if dark {
            unsafe { NSAppearanceNameDarkAqua }
        } else {
            unsafe { NSAppearanceNameAqua }
        };
        if let Some(appearance) = NSAppearance::appearanceNamed(name) {
            NSApp(mtm).setAppearance(Some(&appearance));
        }
    }
    // 注意：广播给壳页面用独立事件名——若也用 "theme-changed"，后端 emit
    // 会被自己的 listener 再次收到，形成无限循环。
    let _ = app.emit("dsh://theme", serde_json::json!({ "theme": theme }));
}

fn set_status(
    app: &tauri::AppHandle,
    status: &str,
    url: Option<String>,
    error: Option<String>,
    error_kind: Option<&str>,
) {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut guard = state.state.lock().unwrap();
        *guard = DshState {
            status: status.to_string(),
            url: url.clone(),
            error,
            error_kind: error_kind.map(String::from),
        };
        guard.clone()
    };
    let _ = app.emit("dsh://state", snapshot);
}

fn boot_timeout_from_env() -> Duration {
    let secs = std::env::var("DSH_UI_BOOT_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BOOT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// 取当前 dsh 进程 stderr 尾部（截到最后 ~2KB），附加到错误信息里。
fn dsh_error_tail(app: &tauri::AppHandle) -> String {
    let tail = app
        .state::<AppState>()
        .process
        .lock()
        .unwrap()
        .as_ref()
        .map(|process| process.stderr_tail())
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&tail);
    let bytes = text.as_bytes();
    if bytes.len() > 2048 {
        String::from_utf8_lossy(&bytes[bytes.len() - 2048..]).to_string()
    } else {
        text.to_string()
    }
}

fn handle_dsh_event(app: &tauri::AppHandle, event: dsh::DshEvent) {
    match event {
        dsh::DshEvent::Ready { url } => {
            let port = url.parse::<tauri::Url>().ok().and_then(|parsed| parsed.port());
            *app.state::<AppState>().dsh_port.lock().unwrap() = port;
            set_status(app, "ready", Some(url.clone()), None, None);
            if let Some(content) = app.get_webview("content") {
                if let Ok(parsed) = url.parse::<tauri::Url>() {
                    let _ = content.navigate(parsed);
                }
            }
        }
        dsh::DshEvent::BootTimeout => {
            let tail = dsh_error_tail(app);
            set_status(
                app,
                "error",
                None,
                Some(format!("dsh 启动超时。最近日志：\n{tail}")),
                Some("boot_timeout"),
            )
        }
        dsh::DshEvent::Exited { requested: false, .. } => {
            let tail = dsh_error_tail(app);
            set_status(
                app,
                "stopped",
                None,
                Some(format!("dsh 进程已退出。最近日志：\n{tail}")),
                Some("crashed"),
            )
        }
        dsh::DshEvent::Exited { requested: true, .. } => {}
    }
}

/// 应用配置目录（`~/Library/Application Support/com.dsh.ui`）。
fn config_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok()
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> settings::AppConfig {
    config_dir(&app).map(|dir| settings::load(&dir)).unwrap_or_default()
}

/// 保存配置并用新配置重启 dsh。
#[tauri::command]
fn save_config(app: tauri::AppHandle, config: settings::AppConfig) -> Result<(), String> {
    let dir = config_dir(&app).ok_or_else(|| "无法确定配置目录".to_string())?;
    settings::save(&dir, &config).map_err(|error| error.to_string())?;
    if let Some(process) = app.state::<AppState>().process.lock().unwrap().take() {
        process.stop();
    }
    start_dsh(&app);
    Ok(())
}

/// 打开设置窗口（已存在则聚焦）。
#[tauri::command]
fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("dsh-ui 设置")
    .inner_size(520.0, 400.0)
    .resizable(false)
    .build()
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// 设置窗口的"测试"按钮：按当前输入做一次探测，返回人话结果。
#[tauri::command]
fn test_environment(
    dsh_path: Option<String>,
    node_path: Option<String>,
) -> Result<String, String> {
    let overrides = dsh::EnvOverrides {
        dsh: settings::normalize_input(&dsh_path).map(PathBuf::from),
        node: settings::normalize_input(&node_path).map(PathBuf::from),
    };
    let path_env = std::env::var("PATH").ok();
    match dsh::detect_environment(path_env.as_deref(), &overrides) {
        dsh::EnvCheck::Ok(env) => Ok(format!(
            "✅ 就绪\ndsh: {}\nnode: {} (v{})",
            env.dsh_path.display(),
            env.node_path.display(),
            env.node_major
        )),
        dsh::EnvCheck::MissingDsh => {
            Err("未找到可用的 dsh：路径无效、不可执行，或留空时自动探测失败".to_string())
        }
        dsh::EnvCheck::MissingNode => Err("未找到可用的 Node.js（需要 ≥22）".to_string()),
        dsh::EnvCheck::NodeTooOld { major } => {
            Err(format!("Node.js 版本过低（v{major}），dsh 需要 ≥{}", dsh::MIN_NODE_MAJOR))
        }
    }
}

/// 探测环境并 spawn dsh；任何失败都落到 error 态并给出安装指引。
fn start_dsh(app: &tauri::AppHandle) {
    let path_env = std::env::var("PATH").ok();
    let config = config_dir(app)
        .map(|dir| settings::load(&dir))
        .unwrap_or_default();
    let overrides = dsh::EnvOverrides {
        dsh: settings::normalize_input(&config.dsh_path).map(PathBuf::from),
        node: settings::normalize_input(&config.node_path).map(PathBuf::from),
    };
    match dsh::detect_environment(path_env.as_deref(), &overrides) {
        dsh::EnvCheck::Ok(env) => {
            let child_path = dsh::child_path(
                &[
                    env.dsh_path.parent().map(|dir| dir.to_path_buf()).unwrap_or_default(),
                    env.node_path.parent().map(|dir| dir.to_path_buf()).unwrap_or_default(),
                ],
                path_env.as_deref(),
            );
            let config = dsh::DshConfig::real(&env.dsh_path, Some(child_path), boot_timeout_from_env());
            let handle = app.clone();
            let on_event =
                move |event: dsh::DshEvent| handle_dsh_event(&handle, event);
            match dsh::DshProcess::spawn(config, Arc::new(on_event)) {
                Ok(process) => {
                    *app.state::<AppState>().process.lock().unwrap() = Some(process);
                    set_status(app, "starting", None, None, None);
                }
                Err(error) => {
                    set_status(
                        app,
                        "error",
                        None,
                        Some(format!("无法启动 dsh：{error}")),
                        Some("spawn_failed"),
                    )
                }
            }
        }
        dsh::EnvCheck::MissingDsh => set_status(
            app,
            "error",
            None,
            Some("未找到 dsh 命令。".to_string()),
            Some("missing_dsh"),
        ),
        dsh::EnvCheck::MissingNode => set_status(
            app,
            "error",
            None,
            Some("未找到可用的 Node.js。".to_string()),
            Some("missing_node"),
        ),
        dsh::EnvCheck::NodeTooOld { major } => set_status(
            app,
            "error",
            None,
            Some(format!("Node.js 版本过低（v{major}），dsh 需要 ≥{}。", dsh::MIN_NODE_MAJOR)),
            Some("node_too_old"),
        ),
    }
}

/// 按 `terminal_open` 状态重排主窗口的 content / bar 两个 webview。
/// bar 总高 = 按钮条(36) + 终端面板（展开时 320，收起时 0）。
/// 注意：全部用**逻辑坐标**——`set_bounds` 对 Physical 变体的处理与
/// `add_child` 不一致（实测物理值会被再除一次 scale，导致高度减半、
/// 点击命中区域错位），逻辑值由 tauri 内部换算，行为与初始 add_child 一致。
fn relayout(app: &tauri::AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let Ok(scale) = window.scale_factor() else {
        return;
    };
    let Ok(inner) = window.inner_size() else {
        return;
    };
    let inner_logical = inner.to_logical::<f64>(scale);
    let term_height = if app.state::<AppState>().terminal_open.load(Ordering::Relaxed) {
        TERMINAL_HEIGHT
    } else {
        0.0
    };
    let content_height = (inner_logical.height - BAR_HEIGHT - term_height).max(0.0);
    if let Some(content) = app.get_webview("content") {
        let _ = content.set_bounds(tauri::Rect {
            position: tauri::LogicalPosition::new(0.0, 0.0).into(),
            size: tauri::LogicalSize::new(inner_logical.width, content_height).into(),
        });
    }
    if let Some(bar) = app.get_webview("bar") {
        let _ = bar.set_bounds(tauri::Rect {
            position: tauri::LogicalPosition::new(0.0, content_height).into(),
            size: tauri::LogicalSize::new(inner_logical.width, BAR_HEIGHT + term_height).into(),
        });
    }
}

/// 装配主窗口：content（dsh 页面/loading）+ bar（底部状态条）双 webview。
///
/// 布局全手动（不用 auto_resize）：按钮条固定高度 + 终端面板开合无法用
/// 等比缩放表达，窗口变化时由 [`relayout`] 重排。
fn build_main_window(app: &tauri::App) -> tauri::Result<()> {
    let window = WindowBuilder::new(app, "main")
        .title("dsh-ui")
        .inner_size(1200.0, 800.0)
        .min_inner_size(640.0, 480.0)
        .resizable(true);
    #[cfg(target_os = "macos")]
    let window = window
        .hidden_title(true)
        .title_bar_style(tauri::TitleBarStyle::Transparent);
    let window = window.build()?;

    // 初始布局与 relayout 保持同一坐标系（逻辑像素）。
    let inner = window.inner_size()?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let inner_logical = inner.to_logical::<f64>(scale);
    let content_height = (inner_logical.height - BAR_HEIGHT).max(0.0);
    let width = inner_logical.width;

    // content：导航锁定——只放行壳页面与当前 dsh 端口，其余转系统浏览器。
    let app_handle = app.handle().clone();
    let content = WebviewBuilder::new("content", WebviewUrl::App("loading.html".into()))
        .initialization_script(THEME_DETECT_SCRIPT)
        .on_navigation(move |url| {
            let allowed = dsh::is_allowed_navigation(
                url.as_str(),
                *app_handle.state::<AppState>().dsh_port.lock().unwrap(),
            );
            if !allowed {
                let _ = std::process::Command::new("open").arg(url.as_str()).spawn();
            }
            allowed
        });
    window.add_child(
        content,
        tauri::LogicalPosition::new(0.0, 0.0),
        tauri::LogicalSize::new(width, content_height),
    )?;

    let bar = WebviewBuilder::new("bar", WebviewUrl::App("bar.html".into()));
    window.add_child(
        bar,
        tauri::LogicalPosition::new(0.0, content_height),
        tauri::LogicalSize::new(width, BAR_HEIGHT),
    )?;
    Ok(())
}

/// 缩放档位（百分比），Safari 风格；Cmd+=/-/0 与预设菜单共用。
pub const ZOOM_STEPS: &[u32] =
    &[25, 33, 50, 67, 75, 90, 100, 110, 125, 150, 175, 200, 250, 300, 400, 500];

/// 放大：返回第一个大于 current 的档位；已达上限时保持 500。
pub fn next_zoom(current: u32) -> u32 {
    ZOOM_STEPS
        .iter()
        .copied()
        .find(|step| *step > current)
        .unwrap_or(*ZOOM_STEPS.last().expect("non-empty"))
}

/// 缩小：返回最后一个小于 current 的档位；已达下限时保持 25。
pub fn prev_zoom(current: u32) -> u32 {
    ZOOM_STEPS
        .iter()
        .copied()
        .rev()
        .find(|step| *step < current)
        .unwrap_or(*ZOOM_STEPS.first().expect("non-empty"))
}

#[cfg(test)]
mod zoom_tests {
    use super::{next_zoom, prev_zoom, ZOOM_STEPS};

    #[test]
    fn zoom_steps_are_ascending_and_bounded() {
        let mut sorted = ZOOM_STEPS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ZOOM_STEPS.to_vec(), "档位必须严格升序且无重复");
        assert_eq!(ZOOM_STEPS.first(), Some(&25));
        assert_eq!(ZOOM_STEPS.last(), Some(&500));
    }

    #[test]
    fn zoom_in_moves_to_next_step_and_clamps_at_max() {
        assert_eq!(next_zoom(100), 110);
        assert_eq!(next_zoom(90), 100);
        assert_eq!(next_zoom(0), 25);
        assert_eq!(next_zoom(500), 500);
        assert_eq!(next_zoom(490), 500);
    }

    #[test]
    fn zoom_out_moves_to_prev_step_and_clamps_at_min() {
        assert_eq!(prev_zoom(100), 90);
        assert_eq!(prev_zoom(110), 100);
        assert_eq!(prev_zoom(25), 25);
        assert_eq!(prev_zoom(26), 25);
    }
}

/// 停止 dsh 并等待其退出（最多 3 秒），然后真正退出应用。
/// 在专用线程执行等待，避免阻塞主线程；托盘退出与系统退出请求共用。
fn request_quit(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        // 先杀内嵌终端的 shell。
        if let Some(session) = state.pty.lock().unwrap().take() {
            session.kill();
        }
        let process = state.process.lock().unwrap().take();
        if let Some(process) = process {
            // 先关 stdin（wrapper 检测 EOF 自杀 dsh），SIGTERM 作双保险。
            process.close_stdin();
            process.stop();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while process.is_running() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        app.exit(0);
    });
}

/// 构建系统托盘：显示/退出菜单 + 左键点击唤起窗口（移植自 deepseek_app）。
#[cfg(target_os = "macos")]
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show = MenuItemBuilder::with_id("show", "显示 dsh-ui").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

    let img = image::load_from_memory(include_bytes!("../icons/icon.png"))
        .expect("failed to load tray icon")
        .to_rgba8();
    let (width, height) = img.dimensions();
    let icon = tauri::image::Image::new_owned(img.into_raw(), width, height);

    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(window) = app.get_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => request_quit(app.clone()),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(AppState {
            process: Mutex::new(None),
            state: Mutex::new(DshState::default()),
            dsh_port: Mutex::new(None),
            zoom: AtomicU32::new(100),
            terminal_open: std::sync::atomic::AtomicBool::new(false),
            pty: Mutex::new(None),
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "zoom_in" => {
                let state = app.state::<AppState>();
                let next = next_zoom(state.zoom.load(Ordering::Relaxed));
                state.zoom.store(next, Ordering::Relaxed);
                apply_zoom(app, next);
            }
            "zoom_out" => {
                let state = app.state::<AppState>();
                let prev = prev_zoom(state.zoom.load(Ordering::Relaxed));
                state.zoom.store(prev, Ordering::Relaxed);
                apply_zoom(app, prev);
            }
            "zoom_reset" => {
                app.state::<AppState>().zoom.store(100, Ordering::Relaxed);
                apply_zoom(app, 100);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_dsh_state,
            diag_report,
            report_theme,
            restart_dsh,
            open_log_dir,
            get_config,
            save_config,
            open_settings,
            test_environment,
            terminal_toggle,
            pty_write,
            pty_resize
        ])
        .setup(|app| {
            build_main_window(app)?;
            #[cfg(target_os = "macos")]
            {
                build_menu(app)?;
                build_tray(app)?;
            }
            // 页面 emit 的主题事件（检测脚本经 invoke/emit 双通道回报）：
            // 主线程应用原生外观。此处只收页面→后端方向的 "theme-changed"。
            let handle = app.handle().clone();
            app.listen("theme-changed", move |event| {
                let payload: serde_json::Value =
                    serde_json::from_str(event.payload()).unwrap_or_default();
                let theme = payload
                    .get("theme")
                    .and_then(|value| value.as_str())
                    .unwrap_or("light")
                    .to_string();
                let normalized = dsh::normalize_theme(&theme).to_string();
                let callback = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    apply_window_theme(&callback, &normalized);
                });
            });
            apply_window_theme(app.handle(), "light");
            start_dsh(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
            // 主窗口尺寸变化时重排（settings 窗口在 relayout 内被忽略）。
            if let tauri::WindowEvent::Resized { .. } = event {
                if window.label() == "main" {
                    relayout(window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app.get_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                // code None = 关闭/系统触发的退出请求；拦下先清理 dsh 再真退出。
                if code.is_none() {
                    api.prevent_exit();
                    request_quit(app.clone());
                }
            }
        });
}
