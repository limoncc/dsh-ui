//! macOS 原生标题栏控件（参考 chat-app 实现）：挂在标题栏右侧（红绿灯对面），
//! 从左到右依次为 状态 / 日志 / 设置 / 终端 四个原生按钮。
//! 状态按钮点击 = 重启 dsh；其余按钮直调对应 Rust 函数，不经过 webview IPC。

use super::{open_settings_window, open_terminal_window, restart_dsh_impl};
use tauri::Manager;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSButton, NSLayoutAttribute, NSTitlebarAccessoryViewController, NSView};
use objc2_foundation::{NSPoint, NSSize, NSObject as FoundationNSObject, NSObjectProtocol};

/// 控件统一高度、容器高度。
const CONTROL_H: f64 = 24.0;
const GAP: f64 = 6.0;
const CONTAINER_H: f64 = 28.0;

/// 供按钮回调读取的 AppHandle（setup 时设置一次）。
static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// 永久存活的 target（NSButton 的 target 是弱引用，必须泄漏存活到 app 生命周期）。
struct TargetPtr(*mut TitlebarTarget);
unsafe impl Send for TargetPtr {}
unsafe impl Sync for TargetPtr {}
static TARGET: std::sync::OnceLock<TargetPtr> = std::sync::OnceLock::new();

/// 控件句柄（raw pointer，仅主线程读写）。
struct Controls {
    container: *mut AnyObject,
    status_btn: *mut AnyObject,
}
unsafe impl Send for Controls {}
static CONTROLS: std::sync::OnceLock<std::sync::Mutex<Option<Controls>>> =
    std::sync::OnceLock::new();

// ObjC 侧 target：接收按钮 action。
define_class!(
    #[unsafe(super(FoundationNSObject))]
    #[thread_kind = MainThreadOnly]
    struct TitlebarTarget;

    impl TitlebarTarget {
        // 状态按钮：点击重启 dsh。
        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: &AnyObject) {
            if let Some(app) = APP_HANDLE.get() {
                restart_dsh_impl(app);
            }
        }

        #[unsafe(method(logsClicked:))]
        fn logs_clicked(&self, _sender: &AnyObject) {
            if APP_HANDLE.get().is_some() {
                super::open_log_dir_impl();
            }
        }

        #[unsafe(method(settingsClicked:))]
        fn settings_clicked(&self, _sender: &AnyObject) {
            if let Some(app) = APP_HANDLE.get() {
                let _ = open_settings_window(app);
            }
        }

        #[unsafe(method(terminalClicked:))]
        fn terminal_clicked(&self, _sender: &AnyObject) {
            if let Some(app) = APP_HANDLE.get() {
                open_terminal_window(app);
            }
        }
    }

    unsafe impl NSObjectProtocol for TitlebarTarget {}
);

impl TitlebarTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// 生成一个标题栏按钮。
unsafe fn make_button(
    mtm: MainThreadMarker,
    target_obj: *mut AnyObject,
    action: Sel,
    title: &str,
) -> Retained<NSButton> {
    let button = NSButton::new(mtm);
    button.setTitle(&objc2_foundation::NSString::from_str(title));
    button.setTarget(Some(&*target_obj));
    button.setAction(Some(action));
    button.setBordered(false);
    button.sizeToFit();
    button
}

/// 首次挂载（窗口创建后、主线程调用）。
pub fn setup(window: &tauri::Window) -> tauri::Result<()> {
    let _ = APP_HANDLE.set(window.app_handle().clone());
    rebuild(window)
}

/// 重建标题栏控件。必须在主线程执行。
pub fn rebuild(window: &tauri::Window) -> tauri::Result<()> {
    let _ = APP_HANDLE.set(window.app_handle().clone());
    let mtm = MainThreadMarker::new().expect("rebuild runs on main thread");

    // 移除旧的 accessory VC（本应用只挂 1 个）。
    if let Ok(nswin) = window.ns_window() {
        let nswin = nswin as *mut NSObject;
        if !nswin.is_null() {
            unsafe {
                let arr: *mut AnyObject = msg_send![nswin, titlebarAccessoryViewControllers];
                let count: usize = msg_send![arr, count];
                if count > 0 {
                    let _: () = msg_send![nswin, removeTitlebarAccessoryViewControllerAtIndex: 0];
                }
            }
        }
    }

    let target_ptr = TARGET.get_or_init(|| {
        let target = TitlebarTarget::new(mtm);
        let leaked: &'static mut Retained<TitlebarTarget> = Box::leak(Box::new(target));
        TargetPtr((&**leaked) as *const TitlebarTarget as *mut TitlebarTarget)
    });
    let target_obj = target_ptr.0 as *mut AnyObject;

    // 从左到右：状态 / 日志 / 设置 / 终端。
    let status_btn = unsafe {
        make_button(mtm, target_obj, sel!(statusClicked:), "🟡 启动中")
    };
    let logs_btn =
        unsafe { make_button(mtm, target_obj, sel!(logsClicked:), "日志") };
    let settings_btn =
        unsafe { make_button(mtm, target_obj, sel!(settingsClicked:), "设置") };
    let terminal_btn =
        unsafe { make_button(mtm, target_obj, sel!(terminalClicked:), "终端") };

    // 统一高度、垂直居中，从左到右排布，整体贴标题栏右缘。
    let y = (CONTAINER_H - CONTROL_H) / 2.0;
    let widths: Vec<f64> = [&status_btn, &logs_btn, &settings_btn, &terminal_btn]
        .iter()
        .map(|b| b.frame().size.width)
        .collect();
    let total_w: f64 = widths.iter().sum::<f64>() + GAP * (widths.len() - 1) as f64;

    let container = NSView::new(mtm);
    container.setFrameSize(NSSize::new(total_w, CONTAINER_H));
    container.setFrameOrigin(NSPoint::new(0.0, 0.0));

    let mut x = 0.0;
    for (idx, button) in [&status_btn, &logs_btn, &settings_btn, &terminal_btn]
        .into_iter()
        .enumerate()
    {
        button.setFrameSize(NSSize::new(widths[idx], CONTROL_H));
        button.setFrameOrigin(NSPoint::new(x, y));
        container.addSubview(button);
        x += widths[idx] + GAP;
    }

    *CONTROLS.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap() = Some(Controls {
        container: (&*container as *const NSView) as *mut AnyObject,
        status_btn: (&*status_btn as *const NSButton) as *mut AnyObject,
    });

    // 挂到标题栏右侧。
    let vc = NSTitlebarAccessoryViewController::new(mtm);
    vc.setView(&container);
    vc.setLayoutAttribute(NSLayoutAttribute::Right);
    if let Ok(nswin) = window.ns_window() {
        let nswin = nswin as *mut NSObject;
        if !nswin.is_null() {
            let _: () = unsafe { msg_send![nswin, addTitlebarAccessoryViewController: &*vc] };
        }
    }
    Ok(())
}

/// 更新状态按钮文字（"● 已连接" 等）。必须在主线程执行。
pub fn update_status(status_title: &str) {
    let Some(lock) = CONTROLS.get() else {
        return;
    };
    let guard = lock.lock().unwrap();
    if let Some(controls) = guard.as_ref() {
        unsafe {
            let button: &NSButton = &*(controls.status_btn as *const NSButton);
            button.setTitle(&objc2_foundation::NSString::from_str(status_title));
            button.sizeToFit();
            // 更新自身宽度后平移右侧兄弟按钮：直接重建容器内布局。
            let container: &NSView = &*(controls.container as *const NSView);
            let width = button.frame().size.width;
            button.setFrameSize(NSSize::new(width, CONTROL_H));
            button.setFrameOrigin(NSPoint::new(0.0, (CONTAINER_H - CONTROL_H) / 2.0));
            let _ = container;
        }
    }
}

/// 供轮询线程调用的入口：主线程更新状态按钮。
pub fn update_status_on_main(app: &tauri::AppHandle, status_title: String) {
    let handle = app.clone();
    let _ = handle.run_on_main_thread(move || {
        update_status(&status_title);
    });
}
