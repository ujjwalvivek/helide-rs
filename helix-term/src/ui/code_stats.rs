//! Code statistics for a folder, from `scc`, drawn at the bottom of the file tree: what sccc
//! prints, cut down to a few rows. Lines of code by language, then the COCOMO estimate of what it
//! would cost a team to write, and the LOCOMO estimate of what a model would.

use std::path::Path;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Modifier, Rect};
use tui::buffer::Buffer as Surface;

use super::file_tree::icon;
use super::panel_style::Roles;

/// Folders that hold dependencies or build output, left out like sccc does.
const EXCLUDE: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    ".pnpm-store",
    ".yarn",
    ".npm",
    "target",
    "dist",
    "build",
    "out",
    "bin",
    "obj",
    "release",
    "debug",
    ".next",
    ".nuxt",
    ".output",
    ".svelte-kit",
    ".astro",
    ".vercel",
    ".netlify",
    ".cache",
    ".turbo",
    ".parcel-cache",
    ".webpack",
    ".rollup.cache",
    "coverage",
    ".coverage",
    "htmlcov",
    "storybook-static",
    ".docusaurus",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".ipynb_checkpoints",
    ".gradle",
    ".idea",
    ".vscode",
    ".vs",
    ".eclipse",
    "logs",
    "tmp",
    "temp",
    ".tmp",
];

/// Rows the stats take under the tree.
pub const HEIGHT: u16 = 8;

pub struct Lang {
    name: String,
    code: u64,
}

pub struct CodeStats {
    langs: Vec<Lang>,
    files: u64,
    code: u64,
    bytes: u64,
    cost: f64,
    months: f64,
    people: f64,
    llm_cost: f64,
    llm_seconds: f64,
    llm_review_hours: f64,
}

/// Runs scc on `dir`. An error is a sentence to show in place of the stats.
pub async fn read(dir: &Path) -> Result<CodeStats, String> {
    let output = tokio::process::Command::new("scc")
        .args(["--no-gen", "--no-min", "--locomo", "--format", "json2"])
        .arg("--exclude-dir")
        .arg(EXCLUDE.join(","))
        .arg(dir)
        .output()
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => "scc is not installed.".to_string(),
            _ => format!("scc: {err}"),
        })?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    parse(&output.stdout).ok_or_else(|| "scc gave output I could not read.".to_string())
}

fn parse(json: &[u8]) -> Option<CodeStats> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let number = |v: &serde_json::Value, key: &str| v.get(key).and_then(|n| n.as_f64());
    let mut langs = Vec::new();
    let (mut files, mut code, mut bytes) = (0, 0, 0);
    for lang in value.get("languageSummary")?.as_array()? {
        let lang_code = number(lang, "Code").unwrap_or(0.0) as u64;
        files += number(lang, "Count").unwrap_or(0.0) as u64;
        bytes += number(lang, "Bytes").unwrap_or(0.0) as u64;
        code += lang_code;
        langs.push(Lang {
            name: lang.get("Name")?.as_str()?.to_string(),
            code: lang_code,
        });
    }
    langs.sort_by(|a, b| b.code.cmp(&a.code));
    Some(CodeStats {
        langs,
        files,
        code,
        bytes,
        cost: number(&value, "estimatedCost").unwrap_or(0.0),
        months: number(&value, "estimatedScheduleMonths").unwrap_or(0.0),
        people: number(&value, "estimatedPeople").unwrap_or(0.0),
        llm_cost: number(&value, "estimatedLLMCost").unwrap_or(0.0),
        llm_seconds: number(&value, "estimatedLLMGenerationSeconds").unwrap_or(0.0),
        llm_review_hours: number(&value, "estimatedLLMReviewHours").unwrap_or(0.0),
    })
}

/// The tree's icon for a language scc names, through a file name typical of it.
fn lang_icon(name: &str) -> (&'static str, helix_view::graphics::Color) {
    let file = match name {
        "Rust" => "a.rs",
        "Markdown" => "a.md",
        "TOML" => "a.toml",
        "JSON" | "JSONC" | "JSON5" => "a.json",
        "YAML" => "a.yaml",
        "JavaScript" => "a.js",
        "TypeScript" | "TypeScript Typings" => "a.ts",
        "JSX" | "TSX" => "a.tsx",
        "Python" => "a.py",
        "Go" => "a.go",
        "C" | "C Header" => "a.c",
        "C++" | "C++ Header" => "a.cpp",
        "C#" => "a.cs",
        "Java" => "a.java",
        "Kotlin" => "a.kt",
        "Swift" => "a.swift",
        "Zig" => "a.zig",
        "Lua" => "a.lua",
        "Scheme" | "Racket" => "a.scm",
        "Nix" => "a.nix",
        "Shell" | "BASH" | "Zsh" | "Fish" | "Nushell" | "PowerShell" => "a.sh",
        "HTML" => "a.html",
        "CSS" | "Sass" | "SCSS" => "a.css",
        "SVG" => "a.svg",
        "SQL" => "a.sql",
        "XML" => "a.xml",
        "Plain Text" => "a.txt",
        "License" => "license",
        "Makefile" | "Justfile" => "makefile",
        "Dockerfile" => "dockerfile",
        "gitignore" | "gitattributes" => ".gitignore",
        "MSBuild" => "a.csproj",
        "INI" | "KDL" => "a.ini",
        other => return icon(&format!("a.{}", other.to_ascii_lowercase()), false, false),
    };
    icon(file, false, false)
}

/// 4641 → "4,641".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Dollars, short: $3, $950, $135k, $2.4M.
fn dollars(n: f64) -> String {
    if n >= 1e6 {
        format!("${:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("${:.0}k", n / 1e3)
    } else {
        format!("${n:.0}")
    }
}

fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn duration(seconds: f64) -> String {
    if seconds >= 3600.0 {
        format!("{:.1} h", seconds / 3600.0)
    } else {
        format!("{:.0} min", (seconds / 60.0).max(1.0))
    }
}

/// A bar `width` cells long at full, filled to `share` in eighths of a cell.
fn bar(share: f64, width: usize) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (share.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        out.push(PARTS[eighths % 8]);
    }
    out
}

/// Draws the stats into `area`: a rule naming the folder, the totals, the languages with the
/// most code, and the two estimates. `stats` is the error to show when scc failed.
pub fn render(
    stats: Option<&Result<CodeStats, String>>,
    folder: &str,
    area: Rect,
    surface: &mut Surface,
    roles: &Roles,
) {
    let width = area.width.saturating_sub(2) as usize;
    let x = area.x + 1;
    let mut y = area.y;
    let rule = "─".repeat(area.width as usize);
    surface.set_stringn(area.x, y, &rule, area.width as usize, roles.faint);
    let title = format!(" {folder} ");
    surface.set_stringn(x, y, &title, width, roles.accent);
    y += 1;
    let stats = match stats {
        None => {
            surface.set_stringn(x, y, "Counting.", width, roles.faint);
            return;
        }
        Some(Err(err)) => {
            surface.set_stringn(x, y, err, width, roles.faint);
            return;
        }
        Some(Ok(stats)) => stats,
    };
    if stats.code == 0 {
        surface.set_stringn(x, y, "No code here.", width, roles.faint);
        return;
    }
    let totals = format!(
        "{} code · {} files · {}",
        grouped(stats.code),
        grouped(stats.files),
        size(stats.bytes)
    );
    surface.set_stringn(
        x,
        y,
        &totals,
        width,
        roles.text.add_modifier(Modifier::BOLD),
    );
    y += 1;

    // Two estimate rows stay at the bottom; languages get the rows between.
    let room = area.bottom().saturating_sub(y + 2) as usize;
    let name_width = stats
        .langs
        .iter()
        .take(room)
        .map(|lang| lang.name.width())
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(2) / 2);
    let shown = if stats.langs.len() > room {
        room.saturating_sub(1)
    } else {
        room
    };
    for lang in stats.langs.iter().take(shown) {
        let count = grouped(lang.code);
        let (glyph, color) = lang_icon(&lang.name);
        surface.set_string(x, y, glyph, roles.text.fg(color));
        surface.set_stringn(x + 2, y, &lang.name, name_width, roles.muted);
        let bar_x = x + 2 + name_width as u16 + 1;
        let bar_width = width.saturating_sub(2 + name_width + 1 + count.width() + 1);
        let share = lang.code as f64 / stats.code as f64;
        let filled = bar(share, bar_width);
        surface.set_stringn(bar_x, y, &filled, bar_width, roles.accent);
        let count_x = bar_x + filled.width() as u16 + 1;
        surface.set_stringn(count_x, y, &count, width, roles.text);
        y += 1;
    }
    if shown < stats.langs.len() && shown < room {
        let rest: u64 = stats.langs[shown..].iter().map(|lang| lang.code).sum();
        let more = format!("{} more · {}", stats.langs.len() - shown, grouped(rest));
        surface.set_stringn(x, y, &more, width, roles.faint);
    }

    let y = area.bottom().saturating_sub(2);
    let team = format!(
        "team {} · {:.1} mo · {:.1} people",
        dollars(stats.cost),
        stats.months,
        stats.people
    );
    let model = format!(
        "model {} · {} · review {:.1} h",
        dollars(stats.llm_cost),
        duration(stats.llm_seconds),
        stats.llm_review_hours
    );
    surface.set_stringn(x, y, &team, width, roles.muted);
    surface.set_stringn(x, y + 1, &model, width, roles.muted);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_scc_json2() {
        let json = br#"{"languageSummary":[
            {"Name":"Markdown","Count":4,"Code":380,"Bytes":27650},
            {"Name":"Rust","Count":14,"Code":4198,"Bytes":174716}],
            "estimatedCost":135389.1,"estimatedScheduleMonths":6.43,"estimatedPeople":1.87,
            "estimatedLLMCost":3.04,"estimatedLLMGenerationSeconds":1987.4,
            "estimatedLLMReviewHours":0.77}"#;
        let stats = parse(json).unwrap();
        assert_eq!(stats.code, 4578);
        assert_eq!(stats.files, 18);
        assert_eq!(stats.langs[0].name, "Rust");
        assert_eq!(dollars(stats.cost), "$135k");
        assert_eq!(duration(stats.llm_seconds), "33 min");
        assert_eq!(grouped(4641), "4,641");
        assert_eq!(bar(0.5, 4), "██");
    }
}
