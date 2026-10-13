//! Is a local or self-hosted engine usable right now? Providers that need no key are "ready" as
//! far as keys go, but Tesseract must be installed and Docling, PaddleOCR and vLLM need their
//! server running. A run that finds out only when every call fails wastes the user's time.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// `None` for providers that are not local engines; otherwise whether the engine can be reached,
/// with what to do when it cannot.
pub fn status(provider: &str, model: &str) -> Option<Result<(), String>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match provider {
        "tesseract" => {
            let cmd = env("TESSERACT_CMD").unwrap_or_else(|| "tesseract".into());
            Some(if on_path(&cmd) {
                Ok(())
            } else {
                Err(format!("Tesseract is not installed ('{cmd}' not found): install it (brew install tesseract, apt install tesseract-ocr) or set TESSERACT_CMD"))
            })
        }
        "docling" => Some(server(
            "DOCLING_BASE_URL",
            env("DOCLING_BASE_URL"),
            "http://localhost:5001",
            "start docling-serve (pip install docling-serve && docling-serve run)",
        )),
        "paddleocr" => {
            let (var, specific) = if model.starts_with("vl") {
                ("PADDLEOCR_VL_BASE_URL", env("PADDLEOCR_VL_BASE_URL"))
            } else {
                ("PADDLEOCR_PARSE_BASE_URL", env("PADDLEOCR_PARSE_BASE_URL"))
            };
            let (var, url) = match specific {
                Some(u) => (var, Some(u)),
                None => ("PADDLEOCR_BASE_URL", env("PADDLEOCR_BASE_URL")),
            };
            Some(server(var, url, "http://localhost:8080", "start PaddleOCR serving"))
        }
        "vllm" => Some(server("VLLM_BASE_URL", env("VLLM_BASE_URL"), "http://localhost:8000", "start a vLLM server")),
        _ => None,
    }
}

fn server(var: &str, url: Option<String>, default: &str, how: &str) -> Result<(), String> {
    let url = url.unwrap_or_else(|| default.to_string());
    if reachable(&url) {
        Ok(())
    } else {
        Err(format!("no server answers at {url}: {how}, or set {var}"))
    }
}

/// A TCP connection to the URL's host and port within a second. Enough to tell "not running"
/// from "running"; the call itself reports anything else.
fn reachable(url: &str) -> bool {
    let Some(rest) = url.split_once("://").map(|(_, r)| r) else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default_port = if url.starts_with("https://") { 443 } else { 80 };
    let target = if authority.starts_with('[') {
        if authority.contains("]:") {
            authority.to_string()
        } else {
            format!("{authority}:{default_port}")
        }
    } else if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:{default_port}")
    };
    let Ok(addrs) = target.to_socket_addrs() else { return false };
    addrs.into_iter().any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
}

fn on_path(cmd: &str) -> bool {
    let p = Path::new(cmd);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listening_server_is_reachable_and_a_closed_port_is_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(reachable(&format!("http://127.0.0.1:{port}/v1")));
        drop(listener);
        assert!(!reachable(&format!("http://127.0.0.1:{port}")));
        assert!(!reachable("not a url"));
    }

    #[test]
    fn only_local_engines_have_a_status() {
        assert!(status("reducto", "r-1").is_none());
        assert!(status("docling", "default").is_some());
    }
}
