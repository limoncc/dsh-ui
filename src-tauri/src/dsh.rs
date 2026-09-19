//! dsh 子进程生命周期管理：环境检测、启动、就绪解析与退出清理。
//!
//! 所有可单测的决策都以纯函数呈现；进程管理通过注入配置实现，
//! 集成测试用假 dsh 脚本替代真实 CLI。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// dsh 要求的最低 Node.js major 版本（用到 `node:sqlite` 等）。
pub const MIN_NODE_MAJOR: u32 = 22;

/// GUI 环境（Finder 启动）常见但可能不在继承 PATH 里的候选目录。
pub const EXTRA_BIN_DIRS: &[&str] = &["/usr/local/bin", "/opt/homebrew/bin"];

/// 在 `search_dirs` 中按序查找可执行文件 `name`，命中第一个含执行权限的常规文件。
pub fn find_executable(name: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    #[cfg(unix)]
    fn is_executable(file: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(file)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    fn is_executable(file: &std::path::Path) -> bool {
        std::fs::metadata(file).map(|meta| meta.is_file()).unwrap_or(false)
    }

    search_dirs
        .iter()
        .map(|dir| dir.join(name))
        .find(|file| is_executable(file))
}

/// 把继承的 `PATH` 环境变量拆成候选目录（空串或空缺返回空表，跳过空段）。
pub fn dirs_from_path(path_env: &str) -> Vec<PathBuf> {
    path_env
        .split(':')
        .filter(|segment| !segment.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 构造探测 dsh/node 的目录序列：继承 PATH 优先，其后补 GUI 常见目录。
pub fn search_dirs(path_env: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = path_env.map(dirs_from_path).unwrap_or_default();
    dirs.extend(EXTRA_BIN_DIRS.iter().map(PathBuf::from));
    dirs
}

/// 解析 `node --version` 输出（`v22.3.0` 形态，容忍尾部空白）为 major 版本号。
pub fn parse_node_version(output: &str) -> Option<u32> {
    let rest = output.trim().strip_prefix('v')?;
    let major = rest.split('.').next()?;
    major.parse::<u32>().ok()
}

/// Node major 是否满足 dsh 最低要求。
pub fn node_version_ok(major: u32) -> bool {
    major >= MIN_NODE_MAJOR
}

/// 环境探测产物：dsh 与 node 的绝对路径及 node 版本。
#[derive(Debug, Clone)]
pub struct Environment {
    pub dsh_path: PathBuf,
    pub node_path: PathBuf,
    pub node_major: u32,
}

/// 环境探测结论；错误分支即错误页的指引依据。
#[derive(Debug)]
pub enum EnvCheck {
    Ok(Environment),
    /// 未找到 dsh 可执行文件。
    MissingDsh,
    /// 未找到 node，或其版本输出无法解析。
    MissingNode,
    /// node 版本低于 dsh 要求。
    NodeTooOld { major: u32 },
}

/// 捕获一个可执行文件带参数运行后的 stdout（启动失败或非零退出返回 None）。
fn run_and_capture_output(program: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// 判断路径是否指向一个可执行的常规文件。
fn is_executable_path(file: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(file)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(file).map(|meta| meta.is_file()).unwrap_or(false)
    }
}

/// 探测 override：用户在设置里手动指定的 dsh/node 路径；Some 时跳过自动探测。
#[derive(Debug, Default, Clone)]
pub struct EnvOverrides {
    pub dsh: Option<PathBuf>,
    pub node: Option<PathBuf>,
}

/// webview 导航放行判定。
///
/// 仅放行两类地址：本地壳页面（loading/bar 等 `tauri://localhost` 页面），
/// 以及当前 dsh 实例的 loopback HTTP 页面（`port` 为就绪后记录的端口；
/// 尚未就绪时为 None，此时一切远程导航都拒绝）。其余地址由调用方
/// 转交系统浏览器打开。
pub fn is_allowed_navigation(url: &str, port: Option<u16>) -> bool {
    const SHELL_PREFIXES: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];
    if SHELL_PREFIXES.iter().any(|prefix| url.starts_with(prefix)) {
        return true;
    }
    let Some(port) = port else {
        return false;
    };
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..authority_end].parse::<u16>() == Ok(port)
}

/// 解析主题导航信号 URL（`dsh-theme://dark|light`）；非主题导航返回 None。
pub fn parse_theme_navigation(url: &str) -> Option<&'static str> {
    match url.strip_prefix("dsh-theme://")? {
        "dark" => Some("dark"),
        "light" => Some("light"),
        _ => None,
    }
}

/// 把页面检测到的主题归一化为 "light"/"dark"；未知值按 light 处理。
pub fn normalize_theme(theme: &str) -> &'static str {
    if theme == "dark" {
        "dark"
    } else {
        "light"
    }
}

/// 按设置 override（优先）与 PATH/GUI 常见目录探测 dsh 与 node，并校验 node 版本。
pub fn detect_environment(path_env: Option<&str>, overrides: &EnvOverrides) -> EnvCheck {
    let fallbacks: Vec<PathBuf> = EXTRA_BIN_DIRS.iter().map(PathBuf::from).collect();
    detect_environment_with_dirs(path_env, &fallbacks, overrides)
}

/// [`detect_environment`] 的可注入版本：`extra_dirs` 替代 GUI 常见目录，供测试使用。
///
/// override 指定了路径但不可执行时，视为对应组件缺失（把选择权交回用户修正）。
pub fn detect_environment_with_dirs(
    path_env: Option<&str>,
    extra_dirs: &[PathBuf],
    overrides: &EnvOverrides,
) -> EnvCheck {
    let mut dirs = path_env.map(dirs_from_path).unwrap_or_default();
    dirs.extend_from_slice(extra_dirs);
    let dsh_path = match &overrides.dsh {
        Some(path) if is_executable_path(path) => Some(path.clone()),
        Some(_) => None,
        None => find_executable("dsh", &dirs),
    };
    let Some(dsh_path) = dsh_path else {
        return EnvCheck::MissingDsh;
    };
    let node_path = match &overrides.node {
        Some(path) if is_executable_path(path) => Some(path.clone()),
        Some(_) => None,
        None => find_executable("node", &dirs),
    };
    let node_probe = node_path
        .and_then(|node| run_and_capture_output(&node, &["--version"]).map(|out| (node, out)));
    let Some((node_path, version_output)) = node_probe else {
        return EnvCheck::MissingNode;
    };
    match parse_node_version(&version_output) {
        Some(major) if node_version_ok(major) => {
            EnvCheck::Ok(Environment { dsh_path, node_path, node_major: major })
        }
        Some(major) => EnvCheck::NodeTooOld { major },
        None => EnvCheck::MissingNode,
    }
}

/// 子进程 PATH：探测命中目录放最前，保证 `dsh` shebang 的 `env node` 能找到同一 node。
pub fn child_path(dirs: &[PathBuf], inherited: Option<&str>) -> String {
    let mut parts: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    if let Some(inherited) = inherited {
        parts.push(inherited.to_string());
    }
    parts.join(":")
}

/// dsh 就绪行的固定前缀（`packages/bundle/web-app` announceReady 打印）。
pub const READY_LINE_PREFIX: &str = "dsh web: ";

/// 从一行 stdout 中解析 dsh 的就绪 URL（带 launch token）。
///
/// 就绪行形如 `dsh web: http://127.0.0.1:<port>/?token=<token>`，
/// 行尾可能带 ` (LAN: ...)` 后缀。仅接受 http、loopback、含非空 token 的
/// 首个空白分隔 token，避免误吞散文行或异常地址。
pub fn parse_ready_line(line: &str) -> Option<String> {
    const LOOPBACK_PREFIX: &str = "http://127.0.0.1:";
    const TOKEN_MARK: &str = "/?token=";

    let rest = line.trim_end_matches(['\r', '\n']).strip_prefix(READY_LINE_PREFIX)?;
    let candidate = rest.split_whitespace().next()?;
    let token_at = candidate.find(TOKEN_MARK)?;
    let authority = candidate[..token_at].strip_prefix(LOOPBACK_PREFIX)?;
    // 端口必须是非空数字串（dsh 打印的是实际监听端口）。
    if authority.is_empty() || !authority.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let token = &candidate[token_at + TOKEN_MARK.len()..];
    (!token.is_empty()).then(|| candidate.to_string())
}

/// dsh 子进程的启动配置；测试注入假脚本，生产用 [`DshConfig::real`]。
#[derive(Debug, Clone)]
pub struct DshConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 子进程 PATH（GUI 环境需显式携带探测到的 dsh/node 目录）。
    pub child_path: Option<String>,
    /// 等待就绪行的最长时间，超时报告 [`DshEvent::BootTimeout`] 并终止子进程。
    pub boot_timeout: Duration,
    /// SIGTERM 后等待退出的宽限期，超时升级 SIGKILL。
    pub kill_grace: Duration,
    /// dsh 的 stdout/stderr 日志落盘路径；None 则只收集尾部不落盘。
    pub log_file: Option<PathBuf>,
    /// true = 持有子进程 stdin 写端（防孤儿心跳：APP 死 → 管道 EOF → wrapper 杀 dsh）。
    pub stdin_pipe: bool,
}

/// 生产 wrapper 脚本：后台启动 dsh，stdin EOF（APP 死亡）时杀掉它。
/// 这让「退出 APP 必关 dsh」不依赖任何退出回调——连 kill -9 都覆盖。
pub const DSH_WRAPPER_SCRIPT: &str = r#"
"$1" web --no-open --port 0 &
pid=$!
while IFS= read -r -t 86400 _; do :; done
kill "$pid" 2>/dev/null
wait "$pid"
"#;

impl DshConfig {
    /// 生产配置：经 stdin 守护 wrapper 启动 dsh，`--port 0` 由 OS 选空闲端口。
    pub fn real(dsh: &Path, child_path: Option<String>, boot_timeout: Duration) -> Self {
        DshConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                DSH_WRAPPER_SCRIPT.to_string(),
                "sh".to_string(),
                dsh.display().to_string(),
            ],
            child_path,
            boot_timeout,
            kill_grace: Duration::from_secs(3),
            log_file: log_dir().map(|dir| dir.join("dsh.log")),
            stdin_pipe: true,
        }
    }
}

/// dsh 子进程向壳报告的运行事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DshEvent {
    /// stdout 出现就绪行，url 为带 launch token 的完整地址。
    Ready { url: String },
    /// 启动超时（未在 boot_timeout 内看到就绪行）。
    BootTimeout,
    /// 进程退出；requested 表示是否由本 APP 主动停止（区别于崩溃）。
    Exited { exit_code: Option<i32>, requested: bool },
}

struct Shared {
    child: Mutex<std::process::Child>,
    ready: AtomicBool,
    timeout_reported: AtomicBool,
    stop_requested: AtomicBool,
    exit_reported: AtomicBool,
    /// stderr 尾部环形缓冲（错误页展示用）。
    stderr_tail: Mutex<Vec<u8>>,
    /// 子进程 stdin 写端（stdin_pipe 时持有；drop 即关闭管道）。
    stdin_writer: Mutex<Option<std::process::ChildStdin>>,
}

/// dsh 子进程句柄：spawn 后由内部线程驱动事件，`stop` 负责进程组终止。
#[derive(Clone)]
pub struct DshProcess {
    shared: Arc<Shared>,
    pid: i32,
    kill_grace: Duration,
}

impl DshProcess {
    pub fn spawn(
        config: DshConfig,
        on_event: Arc<dyn Fn(DshEvent) + Send + Sync>,
    ) -> std::io::Result<Self> {
        let mut command = std::process::Command::new(&config.program);
        command.args(&config.args);
        if let Some(path) = &config.child_path {
            command.env("PATH", path);
        }
        if config.stdin_pipe {
            // 防孤儿心跳：持有写端；APP 死亡 → 管道 EOF → wrapper 杀 dsh。
            command.stdin(std::process::Stdio::piped());
        } else {
            command.stdin(std::process::Stdio::null());
        }
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // 独立进程组：退出时可整组终止 dsh 及其子进程。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn()?;
        // process_group(0) 使子进程成为新组长：pgid == pid，整组 kill 用 -pid。
        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdin_writer = child.stdin.take();

        // 日志文件（truncate 每次启动）；目录不存在时放弃落盘，不影响运行。
        let log_file = config.log_file.as_deref().and_then(|path| {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::File::create(path).ok()
        });

        let shared = Arc::new(Shared {
            child: Mutex::new(child),
            ready: AtomicBool::new(false),
            timeout_reported: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            stderr_tail: Mutex::new(Vec::new()),
            stdin_writer: Mutex::new(stdin_writer),
        });

        // stdout 线程：解析就绪行；EOF 后等进程真正退出并报告一次。
        {
            let shared = shared.clone();
            let on_event = on_event.clone();
            thread::spawn(move || {
                for line in BufRead::lines(BufReader::new(stdout)).map_while(Result::ok) {
                    if let Some(url) = parse_ready_line(&line) {
                        if !shared.ready.swap(true, Ordering::SeqCst) {
                            on_event(DshEvent::Ready { url });
                        }
                    }
                }
                let status = shared.child.lock().unwrap().wait();
                if !shared.exit_reported.swap(true, Ordering::SeqCst) {
                    on_event(DshEvent::Exited {
                        exit_code: status.ok().and_then(|st| st.code()),
                        requested: shared.stop_requested.load(Ordering::SeqCst),
                    });
                }
            });
        }

        // stderr 线程：排空管道防阻塞，同时收集尾部并全量落盘。
        {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut log_file = log_file;
                use std::io::Write;
                for line in BufRead::lines(BufReader::new(stderr)).map_while(Result::ok) {
                    if let Some(file) = log_file.as_mut() {
                        let _ = writeln!(file, "{line}");
                    }
                    let mut tail = shared.stderr_tail.lock().unwrap();
                    tail.extend_from_slice(line.as_bytes());
                    tail.push(b'\n');
                    let overflow = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    if overflow > 0 {
                        tail.drain(..overflow);
                    }
                }
            });
        }

        // 超时线程：boot_timeout 内未就绪则报告并终止子进程。
        {
            let shared = shared.clone();
            let on_event = on_event.clone();
            let boot_timeout = config.boot_timeout;
            let kill_grace = config.kill_grace;
            thread::spawn(move || {
                thread::sleep(boot_timeout);
                if !shared.ready.load(Ordering::SeqCst)
                    && !shared.timeout_reported.swap(true, Ordering::SeqCst)
                {
                    on_event(DshEvent::BootTimeout);
                    request_stop(&shared, pid, kill_grace);
                }
            });
        }

        Ok(DshProcess { shared, pid, kill_grace: config.kill_grace })
    }

    /// 请求终止：SIGTERM 整个进程组，宽限期后升级 SIGKILL。幂等。
    pub fn stop(&self) {
        request_stop(&self.shared, self.pid, self.kill_grace);
    }

    /// 子进程是否仍在运行（探测失败时保守视为在运行）。
    pub fn is_running(&self) -> bool {
        self.shared
            .child
            .lock()
            .unwrap()
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(true)
    }

    /// stderr 尾部内容（最多 [`STDERR_TAIL_BYTES`] 字节），供错误页展示。
    pub fn stderr_tail(&self) -> Vec<u8> {
        self.shared.stderr_tail.lock().unwrap().clone()
    }

    /// 关闭子进程 stdin（防孤儿守护的主动触发：wrapper 检测 EOF 后杀 dsh）。
    pub fn close_stdin(&self) {
        *self.shared.stdin_writer.lock().unwrap() = None;
    }
}

/// stderr 尾部环形缓冲上限。
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// dsh 日志目录：`~/Library/Logs/dsh-ui`（macOS 惯例，跟随 HOME）。
pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Logs/dsh-ui"))
}

fn request_stop(shared: &Arc<Shared>, pid: i32, kill_grace: Duration) {
    if shared.stop_requested.swap(true, Ordering::SeqCst) {
        return;
    }
    kill_process_group(pid, libc::SIGTERM);
    // 看门狗：宽限期内仍存活则升级 SIGKILL。
    let probe = Arc::downgrade(shared);
    thread::spawn(move || {
        let deadline = Instant::now() + kill_grace;
        loop {
            let alive = probe
                .upgrade()
                .and_then(|shared| {
                    shared
                        .child
                        .lock()
                        .ok()
                        .map(|mut child| child.try_wait().map_or(true, |st| st.is_none()))
                })
                .unwrap_or(false);
            if !alive || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        kill_process_group(pid, libc::SIGKILL);
    });
}

/// 向进程组发信号：组长 pid 取负即组 id；目标已死时 kill 返回 ESRCH，忽略。
fn kill_process_group(pid: i32, sig: i32) {
    unsafe {
        libc::kill(-pid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dsh-ui-test-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        pub(super) fn path(&self) -> PathBuf {
            self.0.clone()
        }

        /// 放置一个可执行文件并返回其路径。
        pub(super) fn executable(&self, name: &str) -> PathBuf {
            let file = self.0.join(name);
            fs::write(&file, "#!/bin/sh\n").expect("write fake executable");
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
            file
        }

        /// 放置一个不可执行文件并返回其路径。
        pub(super) fn plain_file(&self, name: &str) -> PathBuf {
            let file = self.0.join(name);
            fs::write(&file, "not executable").expect("write plain file");
            file
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn find_executable_hits_first_directory_in_order() {
        let a = TempDir::new("order-a");
        let b = TempDir::new("order-b");
        let in_a = a.executable("dsh");
        let _in_b = b.executable("dsh");
        let dirs = vec![a.path(), b.path()];
        assert_eq!(find_executable("dsh", &dirs), Some(in_a));
    }

    #[test]
    fn find_executable_skips_missing_dirs_and_files() {
        let a = TempDir::new("skip-a");
        let b = TempDir::new("skip-b");
        let in_b = b.executable("dsh");
        let dirs = vec![a.path().join("nonexistent"), b.path()];
        assert_eq!(find_executable("dsh", &dirs), Some(in_b));
    }

    #[test]
    fn find_executable_rejects_non_executable_file() {
        let a = TempDir::new("exec-a");
        let _plain = a.plain_file("dsh");
        let dirs = vec![a.path()];
        assert_eq!(find_executable("dsh", &dirs), None);
    }

    #[test]
    fn find_executable_missing_everywhere_returns_none() {
        let a = TempDir::new("none-a");
        let dirs = vec![a.path()];
        assert_eq!(find_executable("dsh", &dirs), None);
    }

    #[test]
    fn dirs_from_path_splits_on_colon_and_skips_empty_segments() {
        let dirs = dirs_from_path("/a:/b::/c:");
        assert_eq!(dirs, vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]);
    }

    #[test]
    fn dirs_from_path_empty_input_returns_empty() {
        assert!(dirs_from_path("").is_empty());
    }

    #[test]
    fn search_dirs_appends_gui_fallback_dirs_after_inherited_path() {
        let dirs = search_dirs(Some("/a:/b"));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/opt/homebrew/bin"),
            ]
        );
    }

    #[test]
    fn search_dirs_without_path_env_still_has_fallbacks() {
        assert_eq!(
            search_dirs(None),
            vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/opt/homebrew/bin")]
        );
    }

    #[test]
    fn parse_node_version_reads_major_from_v_string() {
        assert_eq!(parse_node_version("v22.3.0\n"), Some(22));
        assert_eq!(parse_node_version("v24.17.0"), Some(24));
        assert_eq!(parse_node_version("v18.20.0"), Some(18));
    }

    #[test]
    fn parse_node_version_rejects_malformed_output() {
        assert_eq!(parse_node_version(""), None);
        assert_eq!(parse_node_version("node v20.1.0"), None);
        assert_eq!(parse_node_version("vv22.0.0"), None);
        assert_eq!(parse_node_version("v22"), Some(22));
        assert_eq!(parse_node_version("vabc.0.0"), None);
    }

    #[test]
    fn node_version_ok_requires_at_least_minimum() {
        assert!(node_version_ok(MIN_NODE_MAJOR));
        assert!(node_version_ok(24));
        assert!(!node_version_ok(MIN_NODE_MAJOR - 1));
    }

    #[test]
    fn child_path_puts_probe_dirs_first_then_inherited() {
        let dirs = vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/opt/homebrew/bin")];
        assert_eq!(
            child_path(&dirs, Some("/bin:/usr/bin")),
            "/usr/local/bin:/opt/homebrew/bin:/bin:/usr/bin"
        );
    }

    #[test]
    fn child_path_without_inherited_is_probe_dirs_only() {
        let dirs = vec![PathBuf::from("/usr/local/bin")];
        assert_eq!(child_path(&dirs, None), "/usr/local/bin");
    }

    #[test]
    fn parse_ready_line_reads_standard_line() {
        let line = "dsh web: http://127.0.0.1:41234/?token=abc123";
        assert_eq!(parse_ready_line(line), Some("http://127.0.0.1:41234/?token=abc123".to_string()));
    }

    #[test]
    fn parse_ready_line_takes_first_url_before_lan_suffix() {
        let line = "dsh web: http://127.0.0.1:41234/?token=t (LAN: http://192.168.1.5:41234/?token=t)";
        assert_eq!(parse_ready_line(line), Some("http://127.0.0.1:41234/?token=t".to_string()));
    }

    #[test]
    fn parse_ready_line_rejects_prose_line() {
        assert_eq!(
            parse_ready_line("dsh web: opening the default browser; pass --no-open to disable"),
            None
        );
    }

    #[test]
    fn parse_ready_line_rejects_non_loopback_host() {
        assert_eq!(parse_ready_line("dsh web: http://192.168.1.5:3080/?token=t"), None);
    }

    #[test]
    fn parse_ready_line_rejects_non_http_scheme() {
        assert_eq!(parse_ready_line("dsh web: https://127.0.0.1:3080/?token=t"), None);
    }

    #[test]
    fn parse_ready_line_rejects_missing_or_empty_token() {
        assert_eq!(parse_ready_line("dsh web: http://127.0.0.1:3080/"), None);
        assert_eq!(parse_ready_line("dsh web: http://127.0.0.1:3080/?token="), None);
    }

    #[test]
    fn parse_ready_line_rejects_non_numeric_port() {
        assert_eq!(parse_ready_line("dsh web: http://127.0.0.1:port/?token=t"), None);
    }

    #[test]
    fn parse_ready_line_rejects_plain_log_lines() {
        assert_eq!(parse_ready_line("[loader] mounted plugin"), None);
        assert_eq!(parse_ready_line(""), None);
        assert_eq!(parse_ready_line("dsh web:"), None);
    }

    #[test]
    fn parse_ready_line_tolerates_trailing_newline() {
        assert_eq!(
            parse_ready_line("dsh web: http://127.0.0.1:1/?token=t\r\n"),
            Some("http://127.0.0.1:1/?token=t".to_string())
        );
    }

    /// 写一个打印指定内容的"假 node"脚本（`--version` 时输出给定的版本行）。
    fn fake_node(dir: &TempDir, version_line: &str) -> PathBuf {
        let file = dir.path().join("node");
        fs::write(&file, format!("#!/bin/sh\necho '{version_line}'\n")).expect("write fake node");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
        file
    }

    #[test]
    fn detect_environment_ok_when_dsh_and_fresh_node_present() {
        let dir = TempDir::new("env-ok");
        dir.executable("dsh");
        fake_node(&dir, "v22.11.0");
        let extra = vec![dir.path()];
        match detect_environment_with_dirs(Some(""), &extra, &EnvOverrides::default()) {
            EnvCheck::Ok(env) => {
                assert_eq!(env.dsh_path, dir.path().join("dsh"));
                assert_eq!(env.node_path, dir.path().join("node"));
                assert_eq!(env.node_major, 22);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn detect_environment_missing_dsh_when_nothing_found() {
        let dir = TempDir::new("env-empty");
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[dir.path()], &EnvOverrides::default()),
            EnvCheck::MissingDsh
        ));
    }

    #[test]
    fn detect_environment_missing_node_when_only_dsh_found() {
        let dir = TempDir::new("env-nonode");
        dir.executable("dsh");
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[dir.path()], &EnvOverrides::default()),
            EnvCheck::MissingNode
        ));
    }

    #[test]
    fn detect_environment_node_too_old() {
        let dir = TempDir::new("env-old");
        dir.executable("dsh");
        fake_node(&dir, "v18.20.0");
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[dir.path()], &EnvOverrides::default()),
            EnvCheck::NodeTooOld { major: 18 }
        ));
    }

    #[test]
    fn detect_environment_unparsable_node_version_treated_as_missing() {
        let dir = TempDir::new("env-garbage");
        dir.executable("dsh");
        fake_node(&dir, "garbage output");
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[dir.path()], &EnvOverrides::default()),
            EnvCheck::MissingNode
        ));
    }

    #[test]
    fn detect_environment_real_machine_has_compatible_node_if_present() {
        // 宽松冒烟：本机若能找到 node，其版本必须满足 dsh 要求。
        if find_executable("node", &search_dirs(None)).is_some() {
            assert!(matches!(detect_environment(None, &EnvOverrides::default()), EnvCheck::Ok(_)));
        }
    }

    #[test]
    fn overrides_take_precedence_over_search_dirs() {
        let auto_dir = TempDir::new("ovr-auto");
        auto_dir.executable("dsh");
        let manual_dir = TempDir::new("ovr-manual");
        let dsh = manual_dir.executable("dsh");
        let node = fake_node(&manual_dir, "v22.11.0");
        let overrides = EnvOverrides {
            dsh: Some(dsh.clone()),
            node: Some(node.clone()),
        };
        // 搜索目录里也有 dsh，但 override 优先。
        match detect_environment_with_dirs(Some(""), &[auto_dir.path()], &overrides) {
            EnvCheck::Ok(env) => {
                assert_eq!(env.dsh_path, dsh);
                assert_eq!(env.node_path, node);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn invalid_dsh_override_reports_missing_even_when_auto_would_hit() {
        let auto_dir = TempDir::new("ovr-bad-auto");
        auto_dir.executable("dsh");
        let overrides = EnvOverrides {
            dsh: Some(auto_dir.path().join("no-such-dsh")),
            node: None,
        };
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[auto_dir.path()], &overrides),
            EnvCheck::MissingDsh
        ));
    }

    #[test]
    fn invalid_node_override_reports_missing_node() {
        let dir = TempDir::new("ovr-bad-node");
        dir.executable("dsh");
        let overrides = EnvOverrides {
            dsh: None,
            node: Some(dir.path().join("no-such-node")),
        };
        assert!(matches!(
            detect_environment_with_dirs(Some(""), &[dir.path()], &overrides),
            EnvCheck::MissingNode
        ));
    }

    #[test]
    fn navigation_allows_shell_pages_regardless_of_port() {
        assert!(is_allowed_navigation("tauri://localhost/loading.html", None));
        assert!(is_allowed_navigation("tauri://localhost/bar.html", Some(3080)));
        assert!(is_allowed_navigation("http://tauri.localhost/bar.html", None));
    }

    #[test]
    fn navigation_allows_only_current_dsh_port_on_loopback() {
        assert!(is_allowed_navigation("http://127.0.0.1:64898/?token=t", Some(64898)));
        assert!(is_allowed_navigation("http://127.0.0.1:64898/sessions/abc", Some(64898)));
        assert!(!is_allowed_navigation("http://127.0.0.1:64899/?token=t", Some(64898)));
        assert!(!is_allowed_navigation("http://127.0.0.1:64898/", None));
    }

    #[test]
    fn navigation_rejects_foreign_hosts_and_schemes() {
        assert!(!is_allowed_navigation("https://example.com/path", Some(64898)));
        assert!(!is_allowed_navigation("http://localhost:64898/", Some(64898)));
        assert!(!is_allowed_navigation("file:///etc/passwd", Some(64898)));
        assert!(!is_allowed_navigation("http://127.0.0.1:port/", Some(64898)));
    }

    #[test]
    fn normalize_theme_maps_unknown_to_light() {
        assert_eq!(normalize_theme("dark"), "dark");
        assert_eq!(normalize_theme("light"), "light");
        assert_eq!(normalize_theme("Dark"), "light");
        assert_eq!(normalize_theme(""), "light");
    }
}

#[cfg(test)]
mod process_tests {
    use super::tests::TempDir;
    use super::{DshConfig, DshEvent, DshProcess};
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const READY_URL: &str = "http://127.0.0.1:45678/?token=testtoken";
    const READY_LINE: &str = "dsh web: http://127.0.0.1:45678/?token=testtoken";

    fn fake_config(script: &str) -> DshConfig {
        DshConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            child_path: None,
            boot_timeout: Duration::from_millis(150),
            kill_grace: Duration::from_millis(300),
            log_file: None,
            stdin_pipe: false,
        }
    }

    #[test]
    fn stderr_is_collected_to_tail_and_log_file() {
        let dir = TempDir::new("stderr-log");
        let log_path = dir.path().join("dsh.log");
        let mut config = fake_config(
            "echo 'boom' >&2; echo 'warning line' >&2; \
             echo 'dsh web: http://127.0.0.1:45678/?token=t'; sleep 30",
        );
        config.log_file = Some(log_path.clone());
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let process = DshProcess::spawn(config, Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        }))
        .expect("spawn fake dsh");

        // 等就绪行出现（说明 stderr 行已被读入）。
        assert_eq!(
            next_event(&rx, Duration::from_secs(5)),
            DshEvent::Ready { url: "http://127.0.0.1:45678/?token=t".to_string() }
        );
        // stderr 线程与 stdout 线程独立，就绪行出现不代表 stderr 已收集完毕：轮询等待。
        let mut tail = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            tail = String::from_utf8(process.stderr_tail()).expect("stderr tail utf8");
            if tail.contains("boom") && tail.contains("warning line") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(tail.contains("boom"), "tail 应包含 stderr 内容，实际：{tail:?}");
        assert!(tail.contains("warning line"));
        // 不包含 stdout 内容（就绪行走 stdout 不进 stderr tail）。
        assert!(!tail.contains("token=t"));
        process.stop();

        let logged = std::fs::read_to_string(&log_path).expect("read log file");
        assert!(logged.contains("boom") && logged.contains("warning line"));
    }

    #[test]
    fn spawn_failure_returns_error() {
        let config = DshConfig {
            program: PathBuf::from("/nonexistent/dsh-binary-for-test"),
            args: vec![],
            child_path: None,
            boot_timeout: Duration::from_millis(100),
            kill_grace: Duration::from_millis(100),
            log_file: None,
            stdin_pipe: false,
        };
        let result = DshProcess::spawn(config, Arc::new(|_event| {}));
        assert!(result.is_err(), "expected spawn error for missing binary");
    }

    fn spawn_fake(script: &str) -> (DshProcess, Receiver<DshEvent>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let process = DshProcess::spawn(
            fake_config(script),
            Arc::new(move |event| {
                let _ = tx.lock().unwrap().send(event);
            }),
        )
        .expect("spawn fake dsh");
        (process, rx)
    }

    fn next_event(rx: &Receiver<DshEvent>, timeout: Duration) -> DshEvent {
        match rx.recv_timeout(timeout) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for event"),
            Err(RecvTimeoutError::Disconnected) => panic!("event channel closed"),
        }
    }

    #[test]
    fn ready_url_is_reported_once_for_repeated_lines() {
        let (process, rx) =
            spawn_fake(&format!("echo '{READY_LINE}'; echo '{READY_LINE}'; sleep 30"));
        assert_eq!(
            next_event(&rx, Duration::from_secs(5)),
            DshEvent::Ready { url: READY_URL.to_string() }
        );
        // 就绪行重复出现不重复上报：500ms 内不应再有事件。
        assert!(
            matches!(rx.recv_timeout(Duration::from_millis(500)), Err(RecvTimeoutError::Timeout)),
            "unexpected extra event after Ready"
        );
        process.stop();
    }

    #[test]
    fn boot_timeout_is_reported_when_no_ready_line() {
        let (process, rx) = spawn_fake("echo 'still booting'; sleep 5");
        assert_eq!(next_event(&rx, Duration::from_secs(2)), DshEvent::BootTimeout);
        process.stop();
    }

    #[test]
    fn immediate_exit_is_reported_with_code_and_not_requested() {
        let (_process, rx) = spawn_fake("echo 'goodbye'; exit 0");
        assert_eq!(
            next_event(&rx, Duration::from_secs(5)),
            DshEvent::Exited { exit_code: Some(0), requested: false }
        );
    }

    #[test]
    fn abnormal_exit_reports_none_code() {
        let (_process, rx) = spawn_fake("exit 7");
        assert_eq!(
            next_event(&rx, Duration::from_secs(5)),
            DshEvent::Exited { exit_code: Some(7), requested: false }
        );
    }

    #[test]
    fn stop_terminates_process_and_reports_requested_exit() {
        let (process, rx) = spawn_fake("sleep 30");
        process.stop();
        match next_event(&rx, Duration::from_secs(5)) {
            DshEvent::Exited { requested: true, .. } => {}
            other => panic!("expected requested exit, got {other:?}"),
        }
        assert!(!process.is_running());
        // 幂等：重复 stop 不 panic。
        process.stop();
    }

    #[test]
    fn stop_escalates_to_sigkill_when_term_is_ignored() {
        // trap 忽略 SIGTERM 的 shell 会在 sleep（无 trap）死后退出；
        // 无论哪条路径退出，都必须落在 requested 退出上。
        let (process, rx) = spawn_fake("trap '' TERM; sleep 30");
        process.stop();
        match next_event(&rx, Duration::from_secs(5)) {
            DshEvent::Exited { requested: true, .. } => {}
            other => panic!("expected requested exit after escalation, got {other:?}"),
        }
        assert!(!process.is_running());
    }
}

#[cfg(test)]
mod stdin_guard_tests {
    use super::{DshConfig, DshEvent, DshProcess};
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 生成「stdin EOF → 杀后台子进程」的 wrapper 脚本（与生产同构）。
    fn wrapper_config() -> DshConfig {
        DshConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                r#"
"$1" marker-arg &
pid=$!
echo 'dsh web: http://127.0.0.1:45678/?token=wrapper'
# stdin EOF（父进程死亡）→ 杀掉后台 dsh
while IFS= read -r -t 86400 _; do :; done
kill "$pid" 2>/dev/null
wait "$pid"
"#
                .to_string(),
                "echo".to_string(),
            ],
            child_path: None,
            boot_timeout: Duration::from_secs(10),
            kill_grace: Duration::from_millis(300),
            log_file: None,
            stdin_pipe: true,
        }
    }

    #[test]
    fn closing_stdin_kills_background_child() {
        let (tx, rx) = channel::<DshEvent>();
        let tx = Mutex::new(tx);
        let process = DshProcess::spawn(
            wrapper_config(),
            Arc::new(move |event| {
                let _ = tx.lock().unwrap().send(event);
            }),
        )
        .expect("spawn wrapper");

        // 等 wrapper 就绪行（证明后台子进程已启动）。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "wrapper never became ready"
            );
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(DshEvent::Ready { .. }) => break,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => panic!("channel closed"),
            }
        }

        // 模拟 APP 死亡：关闭 stdin 写端 → wrapper 检测 EOF 并杀后台子进程。
        process.close_stdin();

        // wrapper 自身随后台子进程退出 → 收到退出事件。
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(DshEvent::Exited { .. }) => {}
            other => panic!("expected exit after stdin close, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod theme_nav_tests {
    use super::parse_theme_navigation;

    #[test]
    fn theme_navigation_url_parses_to_theme() {
        assert_eq!(parse_theme_navigation("dsh-theme://dark"), Some("dark"));
        assert_eq!(parse_theme_navigation("dsh-theme://light"), Some("light"));
    }

    #[test]
    fn theme_navigation_rejects_other_urls_and_themes() {
        assert_eq!(parse_theme_navigation("dsh-theme://blue"), None);
        assert_eq!(parse_theme_navigation("http://127.0.0.1:3080/?token=t"), None);
        assert_eq!(parse_theme_navigation("tauri://localhost/bar.html"), None);
        assert_eq!(parse_theme_navigation(""), None);
    }
}
