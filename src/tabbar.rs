use std::io::{self, Write};

/// Herdr supplies the active cwd. Read its Git HEAD without querying the server.
pub fn format_tabbar() -> io::Result<()> {
    let cwd = std::env::var("HERDR_ACTIVE_PANE_CWD").unwrap_or_default();
    let branch = crate::git::git_branch(std::path::Path::new(&cwd));
    print!(
        "{}",
        compose_tabbar((!cwd.is_empty()).then_some(cwd.as_str()), branch.as_deref())
    );
    io::stdout().flush()
}

pub fn compose_tabbar(cwd: Option<&str>, branch: Option<&str>) -> String {
    let Some(raw_cwd) = cwd.filter(|s| !s.is_empty()) else {
        return branch.unwrap_or("").to_owned();
    };
    let shortened = shorten_path(raw_cwd, 34);
    match branch.filter(|b| !b.is_empty()) {
        Some(b) => format!("{shortened} · {b}"),
        None => shortened,
    }
}

pub fn shorten_path(cwd: &str, max_len: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let trimmed = cwd.trim_end_matches('/');
    let formatted = if !home.is_empty() && trimmed.starts_with(&home) {
        format!("~{}", &trimmed[home.len()..])
    } else {
        trimmed.to_owned()
    };
    if formatted.len() <= max_len || formatted.is_empty() {
        return if formatted.is_empty() {
            "/".into()
        } else {
            formatted
        };
    }

    let mut suffix = formatted.as_str();
    let separators = formatted.bytes().filter(|&byte| byte == b'/').count();
    for _ in 1..separators {
        suffix = suffix.split_once('/').unwrap().1;
        if "…/".len() + suffix.len() <= max_len {
            break;
        }
    }
    format!("…/{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shorten_path() {
        let home = std::env::var("HOME").unwrap_or_default();
        let long_path = format!("{home}/projects/very/long/nested/path/to/repo");
        let short = shorten_path(&long_path, 25);
        assert!(short.starts_with("…/"));
        assert!(short.len() <= 25);
    }
}
