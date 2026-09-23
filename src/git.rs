use std::fs;
use std::path::Path;

pub(crate) fn git_branch(start: &Path) -> Option<String> {
    if !start.is_absolute() {
        return None;
    }
    for dir in start.ancestors() {
        let marker = dir.join(".git");
        let git = if marker.is_dir() {
            marker
        } else if marker.is_file() {
            let contents = fs::read_to_string(marker).ok()?;
            dir.join(contents.trim().strip_prefix("gitdir:")?.trim())
        } else {
            continue;
        };
        let head = fs::read_to_string(git.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
            return Some(branch.to_owned());
        }
        return (head.len() >= 7 && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| head[..7].to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_lookup_handles_relative_gitdirs_and_detached_heads() {
        let root = std::env::temp_dir().join(format!("hps-head-{}", std::process::id()));
        fs::create_dir_all(root.join("checkout/nested")).unwrap();
        fs::create_dir_all(root.join("git")).unwrap();
        fs::write(root.join("checkout/.git"), "gitdir: ../git\n").unwrap();
        fs::write(root.join("git/HEAD"), "ref: refs/heads/topic/branch\n").unwrap();
        assert_eq!(
            git_branch(&root.join("checkout/nested")).as_deref(),
            Some("topic/branch")
        );
        fs::write(root.join("git/HEAD"), "abcdef0123456789\n").unwrap();
        assert_eq!(
            git_branch(&root.join("checkout/nested")).as_deref(),
            Some("abcdef0")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
