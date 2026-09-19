//! 应用配置：dsh/node 路径自定义，持久化为 JSON。
//!
//! 两个路径都支持"留空 = 自动探测"（`normalize_input` 把空白输入归一化为 None）。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 应用配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// 自定义 dsh 可执行文件路径；None = 自动探测。
    #[serde(default)]
    pub dsh_path: Option<String>,
    /// 自定义 node 可执行文件路径；None = 自动探测。
    #[serde(default)]
    pub node_path: Option<String>,
}

/// 配置文件名（位于 app_config_dir 下）。
pub const CONFIG_FILE: &str = "config.json";

/// 读取配置；文件缺失或内容损坏时返回默认值（自动探测）。
pub fn load(dir: &Path) -> AppConfig {
    let Ok(text) = std::fs::read_to_string(dir.join(CONFIG_FILE)) else {
        return AppConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 保存配置（目录不存在则创建）。
pub fn save(dir: &Path, config: &AppConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(dir.join(CONFIG_FILE), text)
}

/// 归一化用户输入：None 或纯空白视为 None（= 自动探测），其余去首尾空白。
pub fn normalize_input(input: &Option<String>) -> Option<String> {
    input
        .as_deref()
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::{load, normalize_input, save, AppConfig};
    use std::path::PathBuf;
    use std::sync::Mutex;

    // 同一测试进程内共享的临时目录序列（目录名带序号避免冲突）。
    static DIR_SEQ: Mutex<u32> = Mutex::new(0);

    fn temp_dir() -> PathBuf {
        let mut seq = DIR_SEQ.lock().unwrap();
        *seq += 1;
        let dir = std::env::temp_dir().join(format!(
            "dsh-ui-settings-test-{}-{}",
            std::process::id(),
            *seq
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_dir();
        let config = AppConfig {
            dsh_path: Some("/opt/tools/dsh".to_string()),
            node_path: None,
        };
        save(&dir, &config).expect("save");
        assert_eq!(load(&dir), config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_dir_returns_default() {
        let dir = temp_dir().join("nonexistent");
        assert_eq!(load(&dir), AppConfig::default());
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{ not json").unwrap();
        assert_eq!(load(&dir), AppConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_missing_directory() {
        let dir = temp_dir().join("a").join("b");
        save(&dir, &AppConfig::default()).expect("save should create dirs");
        assert!(dir.join("config.json").exists());
        let _ = std::fs::remove_dir_all(&temp_dir().parent().unwrap());
    }

    #[test]
    fn normalize_input_treats_blank_as_auto() {
        assert_eq!(normalize_input(&None), None);
        assert_eq!(normalize_input(&Some("   ".to_string())), None);
        assert_eq!(
            normalize_input(&Some("  /opt/dsh  ".to_string())),
            Some("/opt/dsh".to_string())
        );
    }
}
