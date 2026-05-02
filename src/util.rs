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
