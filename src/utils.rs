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
