use std::net::IpAddr;

use anyhow::{Result, bail};
use regex::Regex;

use crate::config::Config;

pub fn parse_join_addr_prefer_ip(
    log: &str,
    marker: &str,
    preferred_ip: Option<IpAddr>,
) -> Option<String> {
    let pattern = format!(r"{}(\S+)", regex::escape(marker));
    let re = Regex::new(&pattern).ok()?;
    let mut fallback = None;
    for captures in re.captures_iter(log) {
        let Some(match_) = captures.get(1) else {
            continue;
        };
        let addr = match_.as_str().to_string();
        if preferred_ip.is_some_and(|ip| multiaddr_uses_ip(&addr, ip)) {
            return Some(addr);
        }
        fallback = Some(addr);
    }
    fallback
}

pub fn rewrite_join_addr(addr: &str, local_ip: IpAddr) -> Result<String> {
    let parts: Vec<&str> = addr.split('/').collect();
    if parts.len() < 4 {
        bail!("invalid multiaddr: {addr}");
    }

    let replacement_protocol = match local_ip {
        IpAddr::V4(_) => "ip4",
        IpAddr::V6(_) => "ip6",
    };
    let replacement_ip = local_ip.to_string();
    let mut rewritten: Vec<String> = parts.iter().map(|part| (*part).to_string()).collect();

    let mut index = 1;
    while index + 1 < rewritten.len() {
        let protocol = rewritten[index].as_str();
        if protocol == "ip4" || protocol == "ip6" {
            let value = rewritten[index + 1].as_str();
            if is_non_routable_listen_ip(value) {
                rewritten[index] = replacement_protocol.to_string();
                rewritten[index + 1] = replacement_ip;
            }
            break;
        }
        index += 2;
    }

    Ok(rewritten.join("/"))
}

pub fn listener_args(config: &Config, port: u16) -> Vec<String> {
    let mut args = if config.webrtc.listener_args.is_empty() {
        vec![
            "--auto-host-webrtc".to_string(),
            "--webrtc-port".to_string(),
            "{port}".to_string(),
            "--auto-play".to_string(),
        ]
    } else {
        config.webrtc.listener_args.clone()
    };
    render_placeholders(&mut args, Some(port), None);
    args.extend(config.game.listener_extra_args.clone());
    args
}

pub fn joiner_args(config: &Config, join_addr: &str) -> Vec<String> {
    let mut args = if config.webrtc.joiner_args.is_empty() {
        vec![
            "--auto-join-webrtc".to_string(),
            "--webrtc-addr".to_string(),
            "{join_addr}".to_string(),
            "--auto-play".to_string(),
            "--headless".to_string(),
        ]
    } else {
        config.webrtc.joiner_args.clone()
    };
    render_placeholders(&mut args, None, Some(join_addr));
    args.extend(config.game.joiner_extra_args.clone());
    args
}

fn render_placeholders(args: &mut [String], port: Option<u16>, join_addr: Option<&str>) {
    for arg in args {
        if let Some(port) = port {
            *arg = arg.replace("{port}", &port.to_string());
        }
        if let Some(join_addr) = join_addr {
            *arg = arg.replace("{join_addr}", join_addr);
        }
    }
}

fn is_non_routable_listen_ip(value: &str) -> bool {
    matches!(value, "0.0.0.0" | "127.0.0.1" | "::" | "::1" | "localhost")
}

fn multiaddr_uses_ip(addr: &str, ip: IpAddr) -> bool {
    let expected_protocol = match ip {
        IpAddr::V4(_) => "ip4",
        IpAddr::V6(_) => "ip6",
    };
    let expected_ip = ip.to_string();
    let parts: Vec<&str> = addr.split('/').collect();
    let mut index = 1;
    while index + 1 < parts.len() {
        if parts[index] == expected_protocol && parts[index + 1] == expected_ip {
            return true;
        }
        index += 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::{Config, GameConfig, ProcessConfig, WebRtcConfig};

    use super::*;

    #[test]
    fn parses_last_webrtc_join_addr() {
        let log = "noise\nWEBRTC_JOIN_ADDR=/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/abc\n";
        assert_eq!(
            parse_join_addr_prefer_ip(log, "WEBRTC_JOIN_ADDR=", None).as_deref(),
            Some("/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/abc")
        );
    }

    #[test]
    fn prefers_configured_local_ip_when_multiple_addrs_are_logged() {
        let log = "\
WEBRTC_JOIN_ADDR=/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiHash
WEBRTC_JOIN_ADDR=/ip4/10.88.0.1/udp/27200/webrtc-direct/certhash/uEiHash
";
        assert_eq!(
            parse_join_addr_prefer_ip(log, "WEBRTC_JOIN_ADDR=", Some("127.0.0.1".parse().unwrap()))
                .as_deref(),
            Some("/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiHash")
        );
    }

    #[test]
    fn rewrites_wildcard_addr_and_preserves_certhash() {
        let addr = "/ip4/0.0.0.0/udp/27200/webrtc-direct/certhash/uEiHash";
        let rewritten = rewrite_join_addr(addr, "10.0.0.5".parse().unwrap()).unwrap();
        assert_eq!(
            rewritten,
            "/ip4/10.0.0.5/udp/27200/webrtc-direct/certhash/uEiHash"
        );
    }

    #[test]
    fn renders_configured_listener_and_joiner_args() {
        let config = test_config();
        assert_eq!(
            listener_args(&config, 27200),
            vec!["--listen", "webrtc", "--port", "27200", "--extra-listener"]
        );
        assert_eq!(
            joiner_args(
                &config,
                "/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiHash"
            ),
            vec![
                "--join",
                "/ip4/127.0.0.1/udp/27200/webrtc-direct/certhash/uEiHash",
                "--extra-joiner"
            ]
        );
    }

    fn test_config() -> Config {
        Config {
            game: GameConfig {
                name: "test".into(),
                binary: PathBuf::from("game"),
                build_command: None,
                project_root: None,
                assets_dir: None,
                listener_log: PathBuf::from("listener.log"),
                joiner_log: PathBuf::from("joiner.log"),
                env: Vec::new(),
                listener_extra_args: vec!["--extra-listener".into()],
                joiner_extra_args: vec!["--extra-joiner".into()],
            },
            process: ProcessConfig::default(),
            webrtc: WebRtcConfig {
                listener_args: vec![
                    "--listen".into(),
                    "webrtc".into(),
                    "--port".into(),
                    "{port}".into(),
                ],
                joiner_args: vec!["--join".into(), "{join_addr}".into()],
                ..WebRtcConfig::default()
            },
            android: None,
            remote: Vec::new(),
            _steampipe_command: None,
        }
    }
}
