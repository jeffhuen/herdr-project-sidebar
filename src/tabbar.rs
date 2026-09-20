use std::io::{self, Write};

/// Herdr supplies the active cwd. Read its Git HEAD without querying the server.
pub fn format_tabbar() -> io::Result<()> {
    let cwd = std::env::var("HERDR_ACTIVE_PANE_CWD").unwrap_or_default();
    let branch = crate::native::git_branch(std::path::Path::new(&cwd));
    print!("{}", compose_tabbar(cwd_opt(&cwd), branch.as_deref()));
    io::stdout().flush()
}

fn cwd_opt(cwd: &str) -> Option<&str> {
    if cwd.is_empty() {
        None
    } else {
        Some(cwd)
    }
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

    let parts: Vec<&str> = formatted.split('/').collect();
    let mut cur = parts.as_slice();
    while cur.len() > 2 {
        cur = &cur[1..];
        let candidate = format!("…/{}", cur.join("/"));
        if candidate.len() <= max_len {
            return candidate;
        }
    }
    format!("…/{}", cur.join("/"))
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
