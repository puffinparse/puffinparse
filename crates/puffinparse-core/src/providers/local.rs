//! Helpers shared by the self-hosted providers (Tesseract, Docling, PaddleOCR, vLLM): fetching a
//! document into memory, base64 for JSON bodies, file-type sniffing, a scratch directory that
//! cleans up after itself, running local tools (`pdftoppm`) and reading image sizes. None of these
//! providers need an API key.

use crate::error::{Error, Result};
use crate::http::{self, Deadline};
use crate::provider;
use crate::types::{DocumentInput, DocumentRequest};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The document bytes: read from disk / taken from memory, or downloaded when the input is a URL
/// (local engines cannot fetch it themselves, and a self-hosted server may not have egress),
/// through the address-filtered [`crate::fetch`] path.
pub(crate) async fn load_or_download(
    provider_name: &str,
    request: &DocumentRequest,
    deadline: &Deadline,
) -> Result<bytes::Bytes> {
    if let Some(data) = provider::load_bytes(&request.input).await? {
        return Ok(data);
    }
    let DocumentInput::Url { url } = &request.input else { unreachable!("load_bytes only returns None for URLs") };
    let retry = http::Retry::new(request.max_retries);
    Ok(crate::fetch::fetch_document(provider_name, url, deadline, retry).await?.data)
}

/// What kind of file the bytes are, from magic numbers first and the filename second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Pdf,
    Image,
    Other,
}

pub(crate) fn sniff(data: &[u8], filename: &str) -> FileKind {
    const IMAGE_MAGIC: &[&[u8]] = &[
        b"\x89PNG",
        b"\xFF\xD8\xFF",
        b"GIF8",
        b"II*\x00",
        b"MM\x00*",
        b"BM",
        b"RIFF", // WebP (RIFF....WEBP)
        b"P1",
        b"P2",
        b"P3",
        b"P4",
        b"P5",
        b"P6",
    ];
    if data.starts_with(b"%PDF") {
        return FileKind::Pdf;
    }
    if IMAGE_MAGIC.iter().any(|m| data.starts_with(m)) {
        return FileKind::Image;
    }
    let ext = Path::new(filename).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => FileKind::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "tif" | "tiff" | "bmp" | "webp" | "pnm" | "pbm" | "pgm" | "ppm" | "jp2" => {
            FileKind::Image
        }
        _ => FileKind::Other,
    }
}

/// Standard base64 (with padding) for JSON request bodies.
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Keep only the pages selected by a 1-based `"1-3,7"` spec (`None` keeps everything).
pub(crate) fn page_selected(ranges: Option<&[(u32, Option<u32>)]>, page: u32) -> bool {
    match ranges {
        None => true,
        Some(r) => r.iter().any(|&(s, e)| page >= s && e.map(|e| page <= e).unwrap_or(true)),
    }
}

/// A private scratch directory under the system temp dir, removed on drop.
#[derive(Debug)]
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(crate) async fn new(prefix: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("puffinparse-{prefix}-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| Error::input(format!("cannot create scratch directory {}: {e}", path.display())))?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---- local tools -------------------------------------------------------------------------------

/// `pdftoppm` exit codes that mean the input is at fault: 1 cannot open/read the PDF (corrupt or not
/// a PDF), 3 PDF permissions, 99 anything else, which in practice is a page range past the end.
/// These are input errors, not retryable: another model would fail on the same file.
pub(crate) const PDFTOPPM_INPUT_EXITS: &[i32] = &[1, 3, 99];

/// Lines of a tool's stderr kept in an error message; a corrupt PDF can produce hundreds.
pub(crate) const STDERR_LINES: usize = 5;

/// The first [`STDERR_LINES`] lines of a tool's stderr, noting how many were dropped.
pub(crate) fn stderr_excerpt(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.trim().lines().collect();
    if lines.len() <= STDERR_LINES {
        return lines.join("\n");
    }
    format!("{}\n... ({} more lines)", lines[..STDERR_LINES].join("\n"), lines.len() - STDERR_LINES)
}

/// Run a local binary with the call's deadline. A non-zero exit carries the start of the tool's
/// stderr; exit codes in `input_exits` are reported as input errors, and 127 (the shell's "command
/// not found", as seen in minimal containers) as a missing binary.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool(
    provider_name: &str,
    cmd: &str,
    args: &[String],
    envs: &[(&str, &str)],
    deadline: &Deadline,
    tool: &str,
    install_hint: &str,
    input_exits: &[i32],
) -> Result<Vec<u8>> {
    let child = tokio::process::Command::new(cmd)
        .envs(envs.iter().copied())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::provider(format!("{tool} binary '{cmd}' not found on PATH: {install_hint}"))
            } else {
                Error::provider(format!("could not start {tool} ('{cmd}'): {e}"))
            }
            .with_provider(provider_name)
        })?;
    let output = match tokio::time::timeout(deadline.remaining(), child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| Error::provider(format!("{tool} failed: {e}")).with_provider(provider_name))?,
        Err(_) => {
            let msg = format!("deadline exceeded while running {tool}");
            return Err(Error::timeout(msg).with_provider(provider_name));
        }
    };
    if !output.status.success() {
        let stderr = stderr_excerpt(&String::from_utf8_lossy(&output.stderr));
        let code = output.status.code();
        let err = match code {
            Some(127) => Error::provider(format!("{tool} binary '{cmd}' could not be run (exit 127): {install_hint}")),
            Some(c) if input_exits.contains(&c) => {
                Error::input(format!("{tool} could not read the input (exit {c}): {stderr}"))
            }
            _ => Error::provider(format!("{tool} exited with {}: {stderr}", output.status)),
        };
        return Err(err.with_provider(provider_name));
    }
    Ok(output.stdout)
}

/// `pdftoppm` names pages `page-1.png` or `page-01.png` (zero-padded to the page count's width).
pub(crate) async fn rendered_pages(dir: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await.map_err(|e| Error::provider(format!("pdftoppm output: {e}")))?;
    while let Some(entry) = rd.next_entry().await.map_err(|e| Error::provider(format!("pdftoppm output: {e}")))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_prefix("page-").and_then(|s| s.strip_suffix(".png")).and_then(|s| s.parse().ok()) {
            out.push((n, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Rasterise a PDF with poppler's `pdftoppm -r <dpi> -png` into `scratch`, one PNG per page, and
/// return `(page number, path)` for the pages `ranges` selects (only the span covering the
/// selection is rendered). `pdftoppm` is `PDFTOPPM_CMD` or `pdftoppm`; callers decide which.
pub(crate) async fn rasterize_pdf(
    provider_name: &str,
    pdftoppm: &str,
    data: &[u8],
    dpi: u32,
    ranges: Option<&[(u32, Option<u32>)]>,
    scratch: &ScratchDir,
    deadline: &Deadline,
) -> Result<Vec<(u32, PathBuf)>> {
    let pdf = scratch.path().join("input.pdf");
    tokio::fs::write(&pdf, data).await.map_err(|e| Error::input(format!("cannot write {}: {e}", pdf.display())))?;
    let mut args = vec!["-r".to_string(), dpi.to_string(), "-png".to_string()];
    if let Some(r) = ranges {
        // Rasterise only the span that covers the selection; pages outside it are skipped below.
        let first = r.iter().map(|(s, _)| *s).min().unwrap_or(1);
        args.extend(["-f".to_string(), first.to_string()]);
        if let Some(last) = r.iter().map(|(_, e)| *e).try_fold(0u32, |acc, e| e.map(|e| acc.max(e))) {
            args.extend(["-l".to_string(), last.to_string()]);
        }
    }
    args.push(pdf.to_string_lossy().into_owned());
    args.push(scratch.path().join("page").to_string_lossy().into_owned());
    run_tool(
        provider_name,
        pdftoppm,
        &args,
        &[],
        deadline,
        "pdftoppm",
        "install poppler-utils (apt install poppler-utils / brew install poppler) or set PDFTOPPM_CMD",
        PDFTOPPM_INPUT_EXITS,
    )
    .await?;
    Ok(rendered_pages(scratch.path()).await?.into_iter().filter(|(n, _)| page_selected(ranges, *n)).collect())
}

/// Pixel `(width, height)` of a PNG or JPEG from its header, without decoding it.
pub(crate) fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") && &data[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        // Walk the JPEG markers to the first start-of-frame (SOF0..SOF15 except DHT/JPG/DAC).
        let mut i = 2usize;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                return None;
            }
            let marker = data[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u32::from(u16::from_be_bytes([data[i + 5], data[i + 6]]));
                let w = u32::from(u16::from_be_bytes([data[i + 7], data[i + 8]]));
                return (w > 0 && h > 0).then_some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_and_jpeg_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1700u32.to_be_bytes());
        png.extend_from_slice(&2200u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((1700, 2200)));
        // SOI, an APP0 segment, then SOF0 with height 600 / width 800.
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x04\x00\x00\xFF\xC0\x00\x11\x08\x02\x58\x03\x20\x03";
        assert_eq!(image_size(jpeg), Some((800, 600)));
        assert_eq!(image_size(b"GIF89a"), None);
    }

    #[test]
    fn stderr_is_trimmed_to_a_few_lines() {
        assert_eq!(stderr_excerpt("  one\ntwo\n"), "one\ntwo");
        let long: String = (1..=300).map(|i| format!("Syntax Error ({i}): Illegal character\n")).collect();
        let short = stderr_excerpt(&long);
        assert_eq!(short.lines().count(), STDERR_LINES + 1);
        assert!(short.ends_with("... (295 more lines)"), "{short}");
    }

    #[test]
    fn sniffs_by_magic_then_extension() {
        assert_eq!(sniff(b"%PDF-1.7", "x.bin"), FileKind::Pdf);
        assert_eq!(sniff(b"\x89PNG\r\n", "scan"), FileKind::Image);
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0", "a.pdf"), FileKind::Image);
        assert_eq!(sniff(b"????", "scan.TIFF"), FileKind::Image);
        assert_eq!(sniff(b"PK\x03\x04", "a.docx"), FileKind::Other);
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn page_selection() {
        let r = [(1, Some(2)), (5, None)];
        assert!(page_selected(Some(&r), 2));
        assert!(!page_selected(Some(&r), 3));
        assert!(page_selected(Some(&r), 9));
        assert!(page_selected(None, 3));
    }

    #[tokio::test]
    async fn scratch_dir_is_removed_on_drop() {
        let dir = ScratchDir::new("test").await.unwrap();
        let p = dir.path().to_path_buf();
        tokio::fs::write(p.join("a.txt"), b"x").await.unwrap();
        drop(dir);
        assert!(!p.exists());
    }
}
