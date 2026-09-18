//! dsh 子进程生命周期管理：环境检测、启动、就绪解析与退出清理。
//!
//! 所有可单测的决策都以纯函数呈现；进程管理通过注入配置实现，
//! 集成测试用假 dsh 脚本替代真实 CLI。

use std::path::PathBuf;

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

/// 子进程 PATH：探测命中目录放最前，保证 `dsh` shebang 的 `env node` 能找到同一 node。
pub fn child_path(dirs: &[PathBuf], inherited: Option<&str>) -> String {
    let mut parts: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    if let Some(inherited) = inherited {
        parts.push(inherited.to_string());
    }
    parts.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dsh-ui-test-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }

        /// 放置一个可执行文件并返回其路径。
        fn executable(&self, name: &str) -> PathBuf {
            let file = self.0.join(name);
            fs::write(&file, "#!/bin/sh\n").expect("write fake executable");
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
            file
        }

        /// 放置一个不可执行文件并返回其路径。
        fn plain_file(&self, name: &str) -> PathBuf {
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
}
