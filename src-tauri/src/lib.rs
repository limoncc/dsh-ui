pub mod dsh;

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::webview::WebviewBuilder;
use tauri::window::WindowBuilder;
use tauri::{Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl};

/// 底部状态条高度（逻辑像素）。
const BAR_HEIGHT: f64 = 36.0;

/// dsh 启动就绪的最长等待时间；可用 `DSH_UI_BOOT_TIMEOUT` 环境变量覆盖（秒）。
const DEFAULT_BOOT_TIMEOUT_SECS: u64 = 30;

/// 壳对前端暴露的 dsh 运行状态。
#[derive(Clone, Serialize, Default)]
struct DshState {
    status: String, // "starting" | "ready" | "stopped" | "error"
    url: Option<String>,
    error: Option<String>,
}

struct AppState {
    process: Mutex<Option<dsh::DshProcess>>,
    state: Mutex<DshState>,
    /// dsh 就绪后记录的实际端口，用于导航放行判定。
    dsh_port: Mutex<Option<u16>>,
}

#[tauri::command]
fn get_dsh_state(state: State<'_, AppState>) -> DshState {
    state.state.lock().unwrap().clone()
}

fn set_status(app: &tauri::AppHandle, status: &str, url: Option<String>, error: Option<String>) {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut guard = state.state.lock().unwrap();
        *guard = DshState {
            status: status.to_string(),
            url: url.clone(),
            error,
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

fn handle_dsh_event(app: &tauri::AppHandle, event: dsh::DshEvent) {
    match event {
        dsh::DshEvent::Ready { url } => {
            let port = url.parse::<tauri::Url>().ok().and_then(|parsed| parsed.port());
            *app.state::<AppState>().dsh_port.lock().unwrap() = port;
            set_status(app, "ready", Some(url.clone()), None);
            if let Some(content) = app.get_webview("content") {
                if let Ok(parsed) = url.parse::<tauri::Url>() {
                    let _ = content.navigate(parsed);
                }
            }
        }
        dsh::DshEvent::BootTimeout => {
            set_status(app, "error", None, Some("dsh 启动超时，请重试或查看日志。".to_string()))
        }
        dsh::DshEvent::Exited { requested: false, .. } => {
            set_status(app, "stopped", None, Some("dsh 进程已退出。".to_string()))
        }
        dsh::DshEvent::Exited { requested: true, .. } => {}
    }
}

/// 探测环境并 spawn dsh；任何失败都落到 error 态并给出安装指引。
fn start_dsh(app: &tauri::AppHandle) {
    let path_env = std::env::var("PATH").ok();
    match dsh::detect_environment(path_env.as_deref()) {
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
                    set_status(app, "starting", None, None);
                }
                Err(error) => {
                    set_status(app, "error", None, Some(format!("无法启动 dsh：{error}")))
                }
            }
        }
        dsh::EnvCheck::MissingDsh => set_status(
            app,
            "error",
            None,
            Some("未找到 dsh 命令。请先安装 Node.js ≥22，然后执行：npm i -g @deepseek-ai/dsh".to_string()),
        ),
        dsh::EnvCheck::MissingNode => set_status(
            app,
            "error",
            None,
            Some("未找到可用的 Node.js（dsh 需要 ≥22）。请安装或升级 Node.js 后重启。".to_string()),
        ),
        dsh::EnvCheck::NodeTooOld { major } => set_status(
            app,
            "error",
            None,
            Some(format!("Node.js 版本过低（v{major}），dsh 需要 ≥{}，请升级。", dsh::MIN_NODE_MAJOR)),
        ),
    }
}

/// 装配主窗口：content（dsh 页面/loading）+ bar（底部状态条）双 webview。
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

    // 布局以物理像素统一计算；auto_resize 让两个 webview 随窗口按比例伸缩。
    let inner = window.inner_size()?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let bar_height = BAR_HEIGHT * scale;
    let content_height = (inner.height as f64 - bar_height).max(0.0);
    let width = inner.width as f64;

    // content：导航锁定——只放行壳页面与当前 dsh 端口，其余转系统浏览器。
    let app_handle = app.handle().clone();
    let content = WebviewBuilder::new("content", WebviewUrl::App("loading.html".into()))
        .auto_resize()
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
        PhysicalPosition::new(0.0, 0.0),
        PhysicalSize::new(width, content_height),
    )?;

    let bar = WebviewBuilder::new("bar", WebviewUrl::App("bar.html".into())).auto_resize();
    window.add_child(
        bar,
        PhysicalPosition::new(0.0, content_height),
        PhysicalSize::new(width, bar_height),
    )?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
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
        })
        .invoke_handler(tauri::generate_handler![get_dsh_state])
        .setup(|app| {
            build_main_window(app)?;
            start_dsh(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                // code None = 关闭/托盘触发的退出请求；拦下先清理 dsh 再真退出。
                if code.is_none() {
                    api.prevent_exit();
                    let app = app.clone();
                    std::thread::spawn(move || {
                        let process = app
                            .state::<AppState>()
                            .process
                            .lock()
                            .unwrap()
                            .take();
                        if let Some(process) = process {
                            process.stop();
                            let deadline = std::time::Instant::now() + Duration::from_secs(3);
                            while process.is_running() && std::time::Instant::now() < deadline {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                        app.exit(0);
                    });
                }
            }
        });
}
