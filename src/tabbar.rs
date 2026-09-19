use std::io::{self, Write};

/// Tab-bar entry point: Herdr's tab_bar_right command invokes this with the
/// active context in env. No shared file, no second scheduler, no polling.
pub fn format_tabbar() -> io::Result<()> {
    let cwd = std::env::var("HERDR_ACTIVE_PANE_CWD").unwrap_or_default();
    // Pane cwds can sit anywhere (nested dirs, linked checkouts, even spaces
    // Herdr hasn't opened): longest-prefix across every repo's checkout map.
    let branch = checkout_branch_map()
        .into_iter()
        .filter(|(path, _)| cwd == *path || cwd.starts_with(&format!("{path}/")))
        .max_by_key(|(path, _)| path.len())
        .map(|(_, branch)| branch);
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

/// Every known checkout across repos: spaceless worktrees included.
fn checkout_branch_map() -> std::collections::BTreeMap<String, String> {
    let roots: Vec<String> = crate::ipc::call("workspace.list", serde_json::json!({}))
        .ok()
        .and_then(|list| list.get("workspaces")?.as_array().cloned())
        .map(|spaces| {
            let mut roots: Vec<String> = spaces
                .iter()
                .filter_map(|w| {
                    w["worktree"]
                        .get("repo_root")?
                        .as_str()
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned)
                })
                .collect();
            roots.sort();
            roots.dedup();
            roots
        })
        .unwrap_or_default();
    crate::ipc::branch_map(&roots)
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
