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
    for marker in &process.pass_markers {
        if log.contains(marker) {
            return Some(LogSignal::Pass(marker.as_str()));
        }
    }

    for marker in &process.fatal_markers {
        if marker == "Graceful shutdown: exit_code=" {
            if log.contains(marker) && !log.contains("Graceful shutdown: exit_code=0") {
                return Some(LogSignal::Fatal(marker.as_str()));
            }
        } else if log.contains(marker) {
            return Some(LogSignal::Fatal(marker.as_str()));
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
