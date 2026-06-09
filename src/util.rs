use std::path::Path;

pub fn command_line(binary: &Path, args: &[String]) -> String {
    format!("{} {}", binary.display(), shell_args(args))
}

pub fn shell_args(args: &[String]) -> String {
    args.iter().map(shell_quote).collect::<Vec<_>>().join(" ")
}

pub fn shell_quote(value: impl AsRef<str>) -> String {
    let value = value.as_ref();
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn command_line_formats_binary_and_args() {
        let binary = Path::new("game");
        let args = vec!["--arg1".into(), "value".into()];
        assert_eq!(command_line(binary, &args), "game --arg1 value");
    }

    #[test]
    fn shell_args_empty_slice() {
        let args: Vec<String> = vec![];
        assert_eq!(shell_args(&args), "");
    }

    #[test]
    fn shell_args_single_arg() {
        let args = vec!["hello".into()];
        assert_eq!(shell_args(&args), "hello");
    }

    #[test]
    fn shell_args_multiple_args() {
        let args = vec!["--port".into(), "8080".into()];
        assert_eq!(shell_args(&args), "--port 8080");
    }

    #[test]
    fn shell_quote_safe_chars_no_quoting() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("--flag"), "--flag");
        assert_eq!(shell_quote("./path"), "./path");
        assert_eq!(shell_quote("key=value"), "key=value");
    }

    #[test]
    fn shell_quote_spaces_wrap_in_quotes() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_embedded_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }
}
