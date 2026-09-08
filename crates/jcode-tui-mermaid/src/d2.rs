//! Optional local D2/TALA renderer.
//!
//! D2 is deliberately kept behind a feature and invoked as a local CLI. The
//! adapter returns the same PNG-backed `RenderResult` used by Mermaid, so the
//! terminal image, cache, and viewport code remain shared.

use super::{RenderResult, register_active_diagram, register_external_image};
use image::GenericImageView;
use mermaid_rs_renderer::config::RenderConfig;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::tempdir;
use wait_timeout::ChildExt;

const D2_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;
const D2_LAYOUT: &str = "tala";
const D2_ADAPTER_VERSION: &str = "d2-cli-v1";

pub fn is_d2_lang(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "d2")
}

pub fn render_d2_sized(
    content: &str,
    terminal_width: Option<u16>,
    register_active: bool,
) -> RenderResult {
    render_d2(content, terminal_width, register_active)
}

pub fn render_d2_untracked(content: &str, terminal_width: Option<u16>) -> RenderResult {
    render_d2(content, terminal_width, false)
}

fn render_d2(content: &str, terminal_width: Option<u16>, register_active: bool) -> RenderResult {
    if let Err(message) = validate_source(content) {
        return RenderResult::Error(message);
    }

    let cache_path = d2_cache_path(content, terminal_width);
    if let Some(result) = cached_result(&cache_path, register_active) {
        return result;
    }

    let d2_binary = std::env::var_os("JCODE_D2_BIN").unwrap_or_else(|| "d2".into());
    let temp = match tempdir() {
        Ok(temp) => temp,
        Err(error) => return RenderResult::Error(format!("D2 temp directory error: {error}")),
    };
    let input = temp.path().join("input.d2");
    let output = temp.path().join("output.svg");
    if let Err(error) = std::fs::write(&input, content) {
        return RenderResult::Error(format!("D2 input write error: {error}"));
    }

    let mut child = match Command::new(&d2_binary)
        .arg("--layout=tala")
        .arg(&input)
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return RenderResult::Error(format!(
                "D2 renderer unavailable ({}): {error}",
                d2_binary.to_string_lossy()
            ));
        }
    };

    let status = match child.wait_timeout(D2_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return RenderResult::Error("D2 rendering timed out after 10 seconds".to_string());
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return RenderResult::Error(format!("D2 process wait error: {error}"));
        }
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let stderr = stderr.trim();
        return RenderResult::Error(if stderr.is_empty() {
            format!("D2 renderer exited with {status}")
        } else {
            format!("D2 renderer exited with {status}: {stderr}")
        });
    }

    let svg = match read_bounded(&output, MAX_SVG_BYTES) {
        Ok(svg) => svg,
        Err(error) => return RenderResult::Error(format!("D2 SVG output error: {error}")),
    };
    if !svg.contains("<svg") {
        return RenderResult::Error("D2 renderer returned invalid SVG output".to_string());
    }

    if let Some(parent) = cache_path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return RenderResult::Error(format!("D2 cache directory error: {error}"));
    }
    let theme = super::content_render::terminal_theme();
    let render_config = RenderConfig {
        width: 2400.0,
        height: 1800.0,
        background: theme.background.clone(),
    };
    if let Err(error) =
        super::svg::write_output_png_cached_fonts(&svg, &cache_path, &render_config, &theme)
    {
        return RenderResult::Error(format!("D2 SVG-to-PNG error: {error}"));
    }

    cached_result(&cache_path, register_active)
        .unwrap_or_else(|| RenderResult::Error("D2 PNG output could not be registered".to_string()))
}

fn validate_source(content: &str) -> Result<(), String> {
    if content.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "D2 source is too large ({} bytes, maximum {})",
            content.len(),
            MAX_SOURCE_BYTES
        ));
    }
    if content.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("import ") || line.contains("http://") || line.contains("https://")
    }) {
        return Err("D2 imports and external URLs are disabled in jcode".to_string());
    }
    Ok(())
}

fn d2_cache_path(content: &str, terminal_width: Option<u16>) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    D2_ADAPTER_VERSION.hash(&mut hasher);
    D2_LAYOUT.hash(&mut hasher);
    terminal_width.hash(&mut hasher);
    content.hash(&mut hasher);
    let hash = hasher.finish();
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("jcode")
        .join("mermaid")
        .join(format!("d2_{hash:016x}.png"))
}

fn cached_result(path: &Path, register_active: bool) -> Option<RenderResult> {
    let image = image::open(path).ok()?;
    let (width, height) = image.dimensions();
    let hash = register_external_image(path, width, height);
    if register_active {
        register_active_diagram(hash, width, height, None);
    }
    Some(RenderResult::Image {
        hash,
        path: path.to_path_buf(),
        width,
        height,
    })
}

fn read_bounded(path: &Path, max_bytes: u64) -> io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("output is {} bytes, maximum is {max_bytes}", metadata.len()),
        ));
    }
    std::fs::read_to_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_d2_fences() {
        assert!(is_d2_lang("d2"));
        assert!(is_d2_lang(" D2 "));
        assert!(!is_d2_lang("mermaid"));
    }

    #[test]
    fn rejects_external_d2_inputs() {
        assert!(validate_source("import ./other.d2").is_err());
        assert!(validate_source("icon: https://example.com/icon.svg").is_err());
        assert!(validate_source("client -> api").is_ok());
    }

    #[test]
    fn renders_with_configured_d2_binary() {
        if std::env::var_os("JCODE_D2_BIN").is_none() {
            return;
        }
        let content = format!("client_{} -> api_{}: request", std::process::id(), "test");
        let result = render_d2_untracked(&content, Some(96));
        match result {
            RenderResult::Image {
                path,
                width,
                height,
                ..
            } => {
                assert!(path.exists());
                assert!(width > 0 && height > 0);
                let _ = std::fs::remove_file(path);
            }
            RenderResult::Error(error) => panic!("configured D2 failed: {error}"),
        }
    }
}
