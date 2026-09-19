//! 内嵌终端的 PTY 后端：spawn 用户 shell，桥接读写与窗口尺寸。
//!
//! 输出经回调交给壳层（base64 编码后 emit 给 xterm.js）；每次 read 直发，
//! 终端输出速率下事件量可接受，洪峰优化（8ms 聚合）留待实测需要时再做。

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

/// PTY 启动参数；测试注入 `/bin/sh -c …`，生产为用户 shell。
#[derive(Debug, Clone)]
pub struct PtyConfig {
    pub shell: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    /// 附加环境变量（如 TERM=xterm-256color）。
    pub extra_env: Vec<(String, String)>,
}

/// 一个存活的 PTY 会话句柄。
pub struct PtySession {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

/// portable-pty 的自有错误类型统一转成 io::Error。
fn io_err<E: std::fmt::Display>(error: E) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

impl PtySession {
    /// spawn shell；`on_output` 在后台读线程被调用，`on_exit` 在进程退出时回调一次。
    pub fn spawn(
        config: PtyConfig,
        on_output: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
        on_exit: Box<dyn FnOnce(Option<u32>) + Send>,
    ) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: config.rows,
                cols: config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)?;
        let mut command = CommandBuilder::new(&config.shell);
        command.args(&config.args);
        command.cwd(&config.cwd);
        for (key, value) in &config.extra_env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).map_err(io_err)?;
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(io_err)?;
        let writer = pair.master.take_writer().map_err(io_err)?;

        // 读线程：阻塞读 → 回调；EOF 后等待退出码并回调一次。
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => on_output(buffer[..n].to_vec()),
                    Err(_) => break,
                }
            }
        });
        thread::spawn(move || {
            let mut child = child;
            let code = child.wait().ok().map(|status| status.exit_code());
            on_exit(code);
        });

        Ok(Self {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        })
    }

    /// 向 shell 写入（键盘输入）。
    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        self.writer.lock().unwrap().write_all(data)
    }

    /// 调整终端尺寸（前端 fit 后同步）。
    pub fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)
    }

    /// 终止 shell（退出 APP / 关闭面板时）。
    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }
}

#[cfg(test)]
mod tests {
    use super::{PtyConfig, PtySession};
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::sync::Arc;
    use std::time::Duration;

    fn config(script: &str) -> PtyConfig {
        PtyConfig {
            shell: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: std::env::temp_dir(),
            rows: 24,
            cols: 80,
            extra_env: Vec::new(),
        }
    }

    /// 累计输出直到包含目标片段（PTY 回显与 \r\n 转换不必关心）。
    fn wait_for_output(rx: &Receiver<Vec<u8>>, needle: &str, timeout: Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        let mut collected = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for {needle:?}, got {:?}", String::from_utf8_lossy(&collected));
            }
            match rx.recv_timeout(remaining) {
                Ok(bytes) => {
                    collected.extend_from_slice(&bytes);
                    if window_contains(&collected, needle) {
                        return String::from_utf8_lossy(&collected).to_string();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {needle:?}")
                }
                Err(RecvTimeoutError::Disconnected) => panic!("output channel closed"),
            }
        }
    }

    fn window_contains(haystack: &[u8], needle: &str) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn pty_reads_output_writes_input_and_reports_exit() {
        let (tx, rx) = channel::<Vec<u8>>();
        let (exit_tx, exit_rx) = channel::<Option<u32>>();
        let session = PtySession::spawn(
            config("echo pty-ready; cat"),
            Arc::new(move |bytes| {
                let _ = tx.send(bytes);
            }),
            Box::new(move |code| {
                let _ = exit_tx.send(code);
            }),
        )
        .expect("spawn pty");

        // 1. 读到 shell 的输出。
        wait_for_output(&rx, "pty-ready", Duration::from_secs(5));
        // 2. 键盘输入经 PTY 回显（cat 转发）。
        session.write(b"typed-echo\r\n").expect("write");
        wait_for_output(&rx, "typed-echo", Duration::from_secs(5));
        // 3. resize 不 panic。
        session.resize(30, 100).expect("resize");
        // 4. kill 后收到退出回调。
        session.kill();
        match exit_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(_) => {}
            other => panic!("expected exit event, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod real_shell_tests {
    use super::{PtyConfig, PtySession};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 真实 zsh 交互 shell：2 秒内应产生输出（提示符）。
    #[test]
    fn real_zsh_interactive_produces_output() {
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let collected2 = collected.clone();
        let session = PtySession::spawn(
            PtyConfig {
                shell: PathBuf::from("/bin/zsh"),
                args: vec!["-f".to_string()],
                cwd: std::env::temp_dir(),
                rows: 24,
                cols: 80,
                extra_env: Vec::new(),
            },
            Arc::new(move |bytes| {
                collected2.lock().unwrap().extend_from_slice(&bytes);
            }),
            Box::new(|_| {}),
        )
        .expect("spawn real zsh");

        std::thread::sleep(Duration::from_secs(2));
        let out = collected.lock().unwrap().clone();
        session.kill();
        assert!(
            !out.is_empty(),
            "真实 zsh 交互模式应有输出（提示符），实际 0 字节"
        );
    }
}

