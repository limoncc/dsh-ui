//! 验证 tauri remote capability 的 URL 模式对随机端口 loopback 的匹配。

use tauri_utils::acl::RemoteUrlPattern;
use url::Url;

#[test]
fn port_wildcard_matches_random_loopback_port() {
    let pattern: RemoteUrlPattern = "http://127.0.0.1:*".parse().expect("parse pattern");
    let url: Url = "http://127.0.0.1:55517/?token=abc".parse().expect("parse url");
    assert!(pattern.test(&url), "http://127.0.0.1:* 应匹配任意端口的 loopback URL");
}

#[test]
fn port_wildcard_matches_default_port() {
    let pattern: RemoteUrlPattern = "http://127.0.0.1:*".parse().expect("parse pattern");
    let url: Url = "http://127.0.0.1/?token=abc".parse().expect("parse url");
    assert!(pattern.test(&url));
}

#[test]
fn pattern_rejects_other_hosts() {
    let pattern: RemoteUrlPattern = "http://127.0.0.1:*".parse().expect("parse pattern");
    let url: Url = "http://192.168.1.5:55517/".parse().expect("parse url");
    assert!(!pattern.test(&url));
}
