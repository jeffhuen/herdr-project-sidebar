//! Bounded newline-delimited Herdr IPC, without a CLI process per request.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

pub fn call(method: &str, params: Value) -> io::Result<Value> {
    let socket = std::env::var_os("HERDR_SOCKET_PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HERDR_SOCKET_PATH is not set"))?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let id = format!("herdr-project-sidebar:{method}");
    let request = json!({"id": id, "method": method, "params": params});
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    read_response(stream, &id)
}

fn read_response(stream: impl Read, id: &str) -> io::Result<Value> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    let mut line = String::new();
    BufReader::new(stream.take(LIMIT)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated Herdr response",
        ));
    }
    let response: Value = serde_json::from_str(&line)?;
    if response.get("id").and_then(Value::as_str) != Some(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected Herdr response id",
        ));
    }
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Herdr request failed")
                .to_owned(),
        ));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Herdr result"))
}

/// checkout path -> branch across repos, via native worktree.list. One socket
/// round trip per repo root; callers cache (branches move rarely). Paths are
/// normalized (no trailing slash) so checkout_path joins match.
pub fn branch_map(roots: &[String]) -> std::collections::BTreeMap<String, String> {
    fn norm(s: &str) -> String {
        s.trim_end_matches('/').to_owned()
    }
    let mut map = std::collections::BTreeMap::new();
    for root in roots {
        // Daemon rejects trailing-slash cwds; normalize the request.
        let beans = call(
            "worktree.list",
            json!({ "cwd": root.trim_end_matches('/') }),
        );
        let Ok(list) = beans else { continue };
        let empty = Vec::new();
        for w in list
            .pointer("/worktrees")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
        {
            let path = w.get("path").and_then(Value::as_str).unwrap_or("");
            let branch = w.get("branch").and_then(Value::as_str).unwrap_or("");
            if !path.is_empty() && !branch.is_empty() {
                map.insert(norm(path), branch.to_owned());
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_failed_mismatched_and_incomplete_responses() {
        assert!(read_response(&b"{\"id\":\"x\",\"result\":{}}\n"[..], "x").is_ok());
        for reply in [
            "{\"id\":\"x\",\"error\":{\"message\":\"pane closed\"}}\n",
            "{\"id\":\"other\",\"result\":{}}\n",
            "{\"id\":\"x\",\"result\":{}}",
            "{\"id\":\"x\"}\n",
        ] {
            assert!(read_response(reply.as_bytes(), "x").is_err());
        }
    }
}
