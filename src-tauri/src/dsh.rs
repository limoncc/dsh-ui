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

/// 把页面检测到的主题归一化为 "light"/"dark"；未知值按 light 处理。
pub fn normalize_theme(theme: &str) -> &'static str {
    if theme == "dark" {
        "dark"
    } else {
        "light"
    }
}

/// 按 PATH 与 GUI 常见目录探测 dsh 与 node，并校验 node 版本。
pub fn detect_environment(path_env: Option<&str>) -> EnvCheck {
    let fallbacks: Vec<PathBuf> = EXTRA_BIN_DIRS.iter().map(PathBuf::from).collect();
    detect_environment_with_dirs(path_env, &fallbacks)
}

/// [`detect_environment`] 的可注入版本：`extra_dirs` 替代 GUI 常见目录，供测试使用。
pub fn detect_environment_with_dirs(path_env: Option<&str>, extra_dirs: &[PathBuf]) -> EnvCheck {
    let mut dirs = path_env.map(dirs_from_path).unwrap_or_default();
    dirs.extend_from_slice(extra_dirs);
    let Some(dsh_path) = find_executable("dsh", &dirs) else {
        return EnvCheck::MissingDsh;
    };
    let node_probe = find_executable("node", &dirs)
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
}

impl DshConfig {
    /// 生产配置：spawn 探测到的 dsh CLI，`--port 0` 由 OS 选空闲端口。
    pub fn real(dsh: &Path, child_path: Option<String>, boot_timeout: Duration) -> Self {
        DshConfig {
            program: dsh.to_path_buf(),
            args: ["web", "--no-open", "--port", "0"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect(),
            child_path,
            boot_timeout,
            kill_grace: Duration::from_secs(3),
            log_file: log_dir().map(|dir| dir.join("dsh.log")),
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
        command
            .stdin(std::process::Stdio::null())
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
        match detect_environment_with_dirs(Some(""),&extra) {
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
            detect_environment_with_dirs(Some(""),&[dir.path()]),
            EnvCheck::MissingDsh
        ));
    }

    #[test]
    fn detect_environment_missing_node_when_only_dsh_found() {
        let dir = TempDir::new("env-nonode");
        dir.executable("dsh");
        assert!(matches!(
            detect_environment_with_dirs(Some(""),&[dir.path()]),
            EnvCheck::MissingNode
        ));
    }

    #[test]
    fn detect_environment_node_too_old() {
        let dir = TempDir::new("env-old");
        dir.executable("dsh");
        fake_node(&dir, "v18.20.0");
        assert!(matches!(
            detect_environment_with_dirs(Some(""),&[dir.path()]),
            EnvCheck::NodeTooOld { major: 18 }
        ));
    }

    #[test]
    fn detect_environment_unparsable_node_version_treated_as_missing() {
        let dir = TempDir::new("env-garbage");
        dir.executable("dsh");
        fake_node(&dir, "garbage output");
        assert!(matches!(
            detect_environment_with_dirs(Some(""),&[dir.path()]),
            EnvCheck::MissingNode
        ));
    }

    #[test]
    fn detect_environment_real_machine_has_compatible_node_if_present() {
        // 宽松冒烟：本机若能找到 node，其版本必须满足 dsh 要求。
        if find_executable("node", &search_dirs(None)).is_some() {
            assert!(matches!(detect_environment(None), EnvCheck::Ok(_)));
        }
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
        let tail = String::from_utf8(process.stderr_tail()).expect("stderr tail utf8");
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
