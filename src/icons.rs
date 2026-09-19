//! Agent codepoints and animation glyphs matching Radar.
use std::{fs, io, path::PathBuf};
use crate::config::IconMode;

pub const FRAMES: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];

pub fn spinner(step: usize) -> &'static str {
    FRAMES[step % FRAMES.len()]
}

pub fn blocked_mark(step: usize) -> &'static str {
    if (step / 5) % 2 == 1 { "·" } else { "?" }
}

pub fn state_mark(state: &str) -> &'static str {
    match state {
        "done" => "✓",
        "blocked" => "?",
        "working" => "○",
        "idle" | "idle_fresh" | "idle_stale" => "·",
        _ => "◌",
    }
}

pub fn logo(agent: &str, mode: IconMode) -> Option<&'static str> {
    let (font, text) = match agent {
        "claude" => ("\u{e1a0}", "§"),
        "codex" => ("\u{e1a1}", "Λ"),
        "opencode" => ("\u{e1a2}", "◇"),
        "omp" => ("\u{e1a3}", "⬡"),
        "cline" => ("\u{e1a4}", "∇"),
        "mastracode" => ("\u{e1a5}", "∑"),
        "kimi" => ("\u{e1a6}", "✨"),
        "kilo" => ("\u{e1a7}", "♟"),
        "maki" => ("\u{e1a8}", "✳"),
        "pi" => ("\u{e1a9}", "π"),
        "hermes" => ("\u{e1aa}", "☪"),
        "cursor" => ("\u{e1ab}", "◆"),
        "copilot" => ("\u{e1ac}", "⊙"),
        "deepseek" => ("\u{e1ad}", "≋"),
        "gemini" => ("\u{e1ae}", "✦"),
        "gpt" => ("\u{e1af}", "✺"),
        "qwen" => ("\u{e1b0}", "Ϙ"),
        "grok" => ("\u{e1b1}", "✖"),
        "agy" => ("\u{e1b2}", "△"),
        "kiro" => ("\u{e1b3}", "Ω"),
        "amp" => ("\u{e1b4}", "Ʌ"),
        "devin" => ("\u{e1b5}", "ꓓ"),
        "qodercli" => ("\u{e1b6}", "Ǫ"),
        "glm" => ("\u{e1b7}", "Ƶ"),
        _ => return None,
    };
    match mode {
        IconMode::Font => Some(font),
        IconMode::Text => Some(text),
        IconMode::None => None,
    }
}

#[allow(dead_code)]
pub fn all_vendors() -> &'static [&'static str] {
    &[
        "claude", "codex", "opencode", "omp", "cline", "mastracode",
        "kimi", "kilo", "maki", "pi", "hermes", "cursor", "copilot",
        "deepseek", "gemini", "gpt", "qwen", "grok", "agy", "kiro",
        "amp", "devin", "qodercli", "glm",
    ]
}

pub fn install() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    let directory = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Fonts")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".local/share"))
            .join("fonts/herdr-project-sidebar")
    };
    fs::create_dir_all(&directory)?;
    let path = directory.join("HerdrAgentIconsCompact-Regular.ttf");
    fs::write(&path, include_bytes!("../assets/HerdrAgentIconsCompact-Regular.ttf"))?;
    if cfg!(target_os = "linux") {
        let result = std::process::Command::new("fc-cache").arg(&directory).output()?;
        if !result.status.success() {
            return Err(io::Error::other(format!("Font copied; fc-cache failed: {}",
                String::from_utf8_lossy(&result.stderr).trim())));
        }
    }
    Ok(path)
}
