pub fn expand_path(path: &str) -> String {
    if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            return home
                .join(path.strip_prefix("~/").unwrap_or(&path[1..]))
                .to_string_lossy()
                .to_string();
        }
    }
    path.to_string()
}

pub fn safe_backticks(content: &str) -> String {
    let max_backticks = content
        .lines()
        .filter(|line| line.trim().chars().all(|c| c == '`'))
        .map(|line| line.trim().len())
        .max()
        .unwrap_or(0);
    "`".repeat((max_backticks + 1).max(3))
}
