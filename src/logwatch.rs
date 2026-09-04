use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::config::ProcessConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSignal<'a> {
    Pass(&'a str),
    Fatal(&'a str),
}

pub fn read_lossy(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    text
}

pub fn classify_log<'a>(process: &'a ProcessConfig, log: &str) -> Option<LogSignal<'a>> {
    for marker in &process.fatal_markers {
        if marker == "Graceful shutdown: exit_code=" {
            if log.contains(marker) && !log.contains("Graceful shutdown: exit_code=0") {
                return Some(LogSignal::Fatal(marker.as_str()));
            }
        } else if log.contains(marker) {
            return Some(LogSignal::Fatal(marker.as_str()));
        }
    }

    for marker in &process.pass_markers {
        if log.contains(marker) {
            return Some(LogSignal::Pass(marker.as_str()));
        }
    }

    None
}

pub fn classify_non_success_log<'a>(process: &'a ProcessConfig, log: &str) -> Option<&'a str> {
    match classify_log(process, log) {
        Some(LogSignal::Fatal(marker)) => Some(marker),
        Some(LogSignal::Pass(_)) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProcessConfig;

    fn test_process_config() -> ProcessConfig {
        ProcessConfig {
            kill_name: None,
            pass_markers: vec!["GAME OVER".into(), "PASS".into()],
            fatal_markers: vec![
                "[FATAL]".into(),
                "panic".into(),
                "Graceful shutdown: exit_code=".into(),
            ],
        }
    }

    #[test]
    fn read_lossy_returns_content_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        std::fs::write(&path, "hello world").unwrap();
        assert_eq!(read_lossy(&path), "hello world");
    }

    #[test]
    fn read_lossy_returns_empty_for_nonexistent_file() {
        let path = Path::new("/nonexistent-path-that-definitely-does-not-exist.log");
        assert_eq!(read_lossy(path), "");
    }

    #[test]
    fn classify_log_finds_pass_marker() {
        let cfg = test_process_config();
        assert_eq!(
            classify_log(&cfg, "GAME OVER"),
            Some(LogSignal::Pass("GAME OVER"))
        );
    }

    #[test]
    fn classify_log_finds_second_pass_marker() {
        let cfg = test_process_config();
        assert_eq!(
            classify_log(&cfg, "PASS: test passed"),
            Some(LogSignal::Pass("PASS"))
        );
    }

    #[test]
    fn fatal_marker_wins_over_an_earlier_pass_marker() {
        let cfg = test_process_config();
        assert_eq!(
            classify_log(&cfg, "GAME OVER\n[FATAL] later failure"),
            Some(LogSignal::Fatal("[FATAL]"))
        );
    }

    #[test]
    fn classify_log_finds_fatal_marker() {
        let cfg = test_process_config();
        assert_eq!(
            classify_log(&cfg, "[FATAL] out of memory"),
            Some(LogSignal::Fatal("[FATAL]"))
        );
    }

    #[test]
    fn classify_log_graceful_shutdown_exit_code_0_not_fatal() {
        let cfg = test_process_config();
        assert_eq!(classify_log(&cfg, "Graceful shutdown: exit_code=0"), None);
    }

    #[test]
    fn classify_log_graceful_shutdown_exit_code_nonzero_is_fatal() {
        let cfg = test_process_config();
        assert_eq!(
            classify_log(&cfg, "Graceful shutdown: exit_code=1"),
            Some(LogSignal::Fatal("Graceful shutdown: exit_code="))
        );
    }

    #[test]
    fn classify_log_no_markers_returns_none() {
        let cfg = test_process_config();
        assert_eq!(classify_log(&cfg, "just normal log output"), None);
    }

    #[test]
    fn classify_non_success_log_returns_fatal_marker() {
        let cfg = test_process_config();
        assert_eq!(
            classify_non_success_log(&cfg, "[FATAL] error"),
            Some("[FATAL]")
        );
    }

    #[test]
    fn classify_non_success_log_returns_none_for_pass() {
        let cfg = test_process_config();
        assert_eq!(classify_non_success_log(&cfg, "GAME OVER"), None);
    }

    #[test]
    fn classify_non_success_log_returns_none_for_no_match() {
        let cfg = test_process_config();
        assert_eq!(classify_non_success_log(&cfg, "some random log"), None);
    }
}
