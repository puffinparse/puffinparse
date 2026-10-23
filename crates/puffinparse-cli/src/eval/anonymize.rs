//! `eval anonymize`: redacted copies of a dataset, made on this machine before any provider sees it.
//!
//! For each document: PDF pages are rendered to JPEG with poppler's `pdftoppm` (JPEG and simple
//! PNG images are used as they are), every page is read with the local Tesseract engine for word
//! boxes, words matching the chosen kinds of personal data or the user's own terms are covered
//! with black boxes, and the boxed pages are rendered again with `pdftoppm`, so the boxes are
//! burned into the pixels, before the final image-only PDF is written. The copy has no text layer
//! and no original image under the boxes (nor the source JPEG's metadata), so nothing hidden under
//! a box survives; providers then read it by OCR, like a scan.
//!
//! Nothing leaves the machine, and the report counts redactions by kind without ever repeating the
//! redacted text. Detection is pattern-based: an agent should look at a few redacted pages (and pass
//! names, companies and addresses as `terms`) before calling the copy anonymous.

use super::dataset::{self, DatasetDoc};
use puffinparse_core::{self as core, DocumentRequest};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Detected by default. `date` and `id` (long letter-and-digit codes) are opt-in: on invoices
/// and forms they are usually the content being tested, not personal data.
pub const DEFAULT_KINDS: &[&str] = &["email", "phone", "card", "iban", "ssn"];
pub const ALL_KINDS: &[&str] = &["email", "phone", "card", "iban", "ssn", "date", "id"];

#[derive(Debug, Clone)]
pub struct Options {
    pub terms: Vec<String>,
    /// Empty: [`DEFAULT_KINDS`].
    pub kinds: Vec<String>,
    pub dpi: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DocReport {
    pub id: String,
    /// Path of the redacted copy, relative to the output folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub pages: usize,
    pub redactions: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub source: String,
    pub output: String,
    pub kinds: Vec<String>,
    pub terms: usize,
    pub documents: usize,
    pub redacted_documents: usize,
    pub redactions: BTreeMap<String, usize>,
    pub docs: Vec<DocReport>,
    pub notes: Vec<String>,
}

// ---- detection -----------------------------------------------------------------------------------

/// A word on a page with its normalised box (top-left origin, 0–1).
#[derive(Debug, Clone)]
pub struct PageWord {
    pub text: String,
    pub bbox: core::BBox,
}

fn digits(s: &str) -> usize {
    s.chars().filter(char::is_ascii_digit).count()
}

fn phoneish(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || "+()-./".contains(c)) && digits(s) > 0
}

fn luhn(number: &str) -> bool {
    let ds: Vec<u32> = number.chars().filter_map(|c| c.to_digit(10)).collect();
    if ds.len() < 13 {
        return false;
    }
    let sum: u32 = ds
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            if i % 2 == 1 {
                if d * 2 > 9 {
                    d * 2 - 9
                } else {
                    d * 2
                }
            } else {
                *d
            }
        })
        .sum();
    sum % 10 == 0
}

fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '@')
}

fn is_email(w: &str) -> bool {
    let w = trim_punct(w);
    match w.split_once('@') {
        Some((user, domain)) => !user.is_empty() && domain.contains('.') && !domain.ends_with('.'),
        None => false,
    }
}

fn is_ssn(w: &str) -> bool {
    let w = trim_punct(w);
    let parts: Vec<&str> = w.split('-').collect();
    parts.len() == 3
        && [3, 2, 4].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_digit()))
}

fn is_iban(s: &str) -> bool {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let b = s.as_bytes();
    (15..=34).contains(&b.len())
        && b[0].is_ascii_uppercase()
        && b[1].is_ascii_uppercase()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn is_date(w: &str) -> bool {
    let w = trim_punct(w);
    let parts: Vec<&str> = w.split(['/', '-', '.']).collect();
    if parts.len() != 3 || !parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let l: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    let short = |n: usize| n == 1 || n == 2;
    // 2026-10-12, 12/10/2026, 12.10.26 (not 123-45-6789).
    (l[0] == 4 && short(l[1]) && short(l[2])) || (short(l[0]) && short(l[1]) && (l[2] == 4 || l[2] == 2))
}

fn is_id(w: &str) -> bool {
    let w = trim_punct(w);
    w.len() >= 8
        && digits(w) >= 4
        && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !w.chars().all(|c| c.is_ascii_digit() || c == '-')
}

fn norm_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Indices of words to redact, with the kind that matched each.
pub fn detect(words: &[PageWord], kinds: &[String], terms: &[String]) -> Vec<(usize, String)> {
    let on = |k: &str| kinds.iter().any(|x| x == k);
    let mut hits: BTreeMap<usize, String> = BTreeMap::new();
    let mark = |range: std::ops::Range<usize>, kind: &str, hits: &mut BTreeMap<usize, String>| {
        for i in range {
            hits.entry(i).or_insert_with(|| kind.to_string());
        }
    };
    // Terms first: the user's own names, companies, addresses (case and punctuation ignored).
    let normed: Vec<String> = words.iter().map(|w| norm_word(&w.text)).collect();
    for term in terms {
        let tw: Vec<String> = term.split_whitespace().map(norm_word).filter(|w| !w.is_empty()).collect();
        if tw.is_empty() {
            continue;
        }
        for start in 0..normed.len() {
            if start + tw.len() <= normed.len() && normed[start..start + tw.len()] == tw[..] {
                mark(start..start + tw.len(), "term", &mut hits);
            }
        }
    }
    for (i, w) in words.iter().enumerate() {
        let t = w.text.as_str();
        if on("email") && is_email(t) {
            mark(i..i + 1, "email", &mut hits);
        } else if on("ssn") && is_ssn(t) {
            mark(i..i + 1, "ssn", &mut hits);
        } else if on("iban") && is_iban(trim_punct(t)) {
            mark(i..i + 1, "iban", &mut hits);
        } else if on("date") && is_date(t) {
            mark(i..i + 1, "date", &mut hits);
        } else if on("id") && is_id(t) {
            mark(i..i + 1, "id", &mut hits);
        }
    }
    // Multi-word numbers: card numbers (Luhn) and phone numbers, as runs of digit-and-separator
    // words; IBANs printed in groups of four.
    let numberish = |w: &PageWord| phoneish(trim_punct(&w.text)) || w.text.starts_with('(');
    // Cards first, from every start word and every window length: a line number before the card or
    // its expiry date after it must not hide it inside a longer run.
    let mut carded = vec![false; words.len()];
    if on("card") {
        for start in 0..words.len() {
            let mut joined = String::new();
            let mut end = None;
            for (j, w) in words.iter().enumerate().skip(start).take(6) {
                if !numberish(w) {
                    break;
                }
                joined.push_str(trim_punct(&w.text));
                if (13..=19).contains(&digits(&joined)) && luhn(&joined) {
                    end = Some(j + 1);
                }
            }
            if let Some(end) = end {
                mark(start..end, "card", &mut hits);
                carded[start..end].iter_mut().for_each(|c| *c = true);
            }
        }
    }
    let mut i = 0;
    while i < words.len() {
        if carded[i] || !numberish(&words[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut joined = String::new();
        while j < words.len() && !carded[j] && numberish(&words[j]) && j - i < 6 {
            joined.push_str(trim_punct(&words[j].text));
            j += 1;
        }
        let n = digits(&joined);
        if on("phone")
            && (8..=15).contains(&n)
            && !is_date(&joined)
            && (j - i > 1 || joined.contains(['+', '(', '-', ' ']) || n >= 10)
        {
            mark(i..j, "phone", &mut hits);
        }
        i = j.max(i + 1);
    }
    if on("iban") {
        for start in 0..words.len() {
            let mut joined = String::new();
            for (end, w) in words.iter().enumerate().skip(start).take(9) {
                joined.push_str(trim_punct(&w.text));
                if end > start && is_iban(&joined) {
                    mark(start..end + 1, "iban", &mut hits);
                }
            }
        }
    }
    hits.into_iter().collect()
}

// ---- images and PDF ------------------------------------------------------------------------------

/// Width and height of a baseline or progressive JPEG, from its SOF marker.
pub fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) || marker == 0xff {
            i += if marker == 0xff { 1 } else { 2 };
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            let comps = data[i + 9];
            return (comps == 1 || comps == 3).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Number of colour components of a JPEG (1 grey, 3 colour).
fn jpeg_components(data: &[u8]) -> u8 {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] == 0xff && matches!(data[i + 1], 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return data[i + 9];
        }
        if data[i] == 0xff && data[i + 1] != 0xff && data[i + 1] != 0xd8 && !(0xd0..=0xd7).contains(&data[i + 1]) {
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            i += 2 + len;
        } else {
            i += 1;
        }
    }
    3
}

/// A page image ready for the PDF: either a JPEG (embedded as is) or an 8-bit grey/RGB PNG
/// (its zlib stream embedded with the PNG predictor).
pub enum PageImage {
    Jpeg { data: Vec<u8>, width: u32, height: u32, components: u8 },
    Png { idat: Vec<u8>, width: u32, height: u32, components: u8 },
}

impl PageImage {
    pub fn size(&self) -> (u32, u32) {
        match self {
            PageImage::Jpeg { width, height, .. } | PageImage::Png { width, height, .. } => (*width, *height),
        }
    }
}

/// Read a PNG the PDF can embed directly: 8 bits per channel, grey or RGB, not interlaced.
pub fn png_image(data: &[u8]) -> Result<PageImage, String> {
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !data.starts_with(SIG) {
        return Err("not a PNG".into());
    }
    let mut i = 8;
    let mut header = None;
    let mut idat = Vec::new();
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let kind = &data[i + 4..i + 8];
        let body = data.get(i + 8..i + 8 + len).ok_or("truncated PNG")?;
        match kind {
            b"IHDR" if len >= 13 => {
                let w = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let h = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                header = Some((w, h, body[8], body[9], body[12]));
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        i += 12 + len;
    }
    let (w, h, depth, color, interlace) = header.ok_or("PNG without a header")?;
    let components = match color {
        0 => 1,
        2 => 3,
        _ => {
            return Err("PNGs with transparency or a palette are not supported; save it as JPEG (or a PDF) first".into())
        }
    };
    if depth != 8 || interlace != 0 {
        return Err("only 8-bit, non-interlaced PNGs are supported; save it as JPEG (or a PDF) first".into());
    }
    Ok(PageImage::Png { idat, width: w, height: h, components })
}

/// An image-only PDF: one page per image, each image drawn full-page at `dpi`, black rectangles
/// (normalised boxes) painted over it.
pub fn write_pdf(pages: &[(PageImage, Vec<core::BBox>)], dpi: u32) -> Vec<u8> {
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets: Vec<usize> = Vec::new();
    let n = pages.len();
    // Objects: 1 catalog, 2 pages, then per page: page, content, image.
    let page_id = |i: usize| 3 + i * 3;
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_id(i))).collect();
    obj(&mut out, &mut offsets, format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" ")).as_bytes());
    let scale = 72.0 / dpi.max(1) as f64;
    for (i, (img, boxes)) in pages.iter().enumerate() {
        let (w, h) = img.size();
        let (pw, ph) = (w as f64 * scale, h as f64 * scale);
        let id = page_id(i);
        obj(
            &mut out,
            &mut offsets,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Resources << /XObject << /Im0 {} 0 R >> >> /Contents {} 0 R >>",
                id + 2,
                id + 1
            )
            .as_bytes(),
        );
        let mut content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q\n0 g\n");
        for b in boxes {
            // Pad each box a little: OCR boxes hug the glyphs.
            let pad = 0.003;
            let (x0, x1) = ((b.x0 - pad).max(0.0), (b.x1 + pad).min(1.0));
            let (y0, y1) = ((b.y0 - pad).max(0.0), (b.y1 + pad).min(1.0));
            content.push_str(&format!(
                "{:.2} {:.2} {:.2} {:.2} re f\n",
                x0 * pw,
                (1.0 - y1) * ph,
                (x1 - x0) * pw,
                (y1 - y0) * ph
            ));
        }
        let mut body = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        body.extend_from_slice(content.as_bytes());
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &body);
        let (dict, data): (String, &[u8]) = match img {
            PageImage::Jpeg { data, width, height, components } => (
                format!(
                    "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /{} /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>",
                    if *components == 1 { "DeviceGray" } else { "DeviceRGB" },
                    data.len()
                ),
                data,
            ),
            PageImage::Png { idat, width, height, components } => (
                format!(
                    "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /{} /BitsPerComponent 8 /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors {components} /BitsPerComponent 8 /Columns {width} >> /Length {} >>",
                    if *components == 1 { "DeviceGray" } else { "DeviceRGB" },
                    idat.len()
                ),
                idat,
            ),
        };
        let mut body = format!("{dict}\nstream\n").into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &body);
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1).as_bytes(),
    );
    out
}

// ---- the run -------------------------------------------------------------------------------------

/// Render a PDF's pages to JPEG files in `scratch` with `pdftoppm`; returns them in page order.
async fn render_pdf(path: &Path, scratch: &Path, dpi: u32) -> Result<Vec<PathBuf>, String> {
    let cmd = std::env::var("PDFTOPPM_CMD").unwrap_or_else(|_| "pdftoppm".into());
    let out = tokio::process::Command::new(&cmd)
        .arg("-r")
        .arg(dpi.to_string())
        .arg("-jpeg")
        .arg(path)
        .arg(scratch.join("page"))
        .output()
        .await
        .map_err(|e| {
            format!("cannot run {cmd} (install poppler: `brew install poppler` or `apt install poppler-utils`): {e}")
        })?;
    if !out.status.success() {
        return Err(format!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(scratch)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
        .collect();
    // page-1.jpg … page-10.jpg (pdftoppm pads to the page count's width, but sort numerically anyway).
    files.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| s.to_string_lossy().rsplit('-').next().and_then(|n| n.parse::<u32>().ok()))
            .unwrap_or(0)
    });
    Ok(files)
}

/// Word boxes on one page image, from the local Tesseract engine.
async fn page_words(image: &Path) -> Result<Vec<PageWord>, String> {
    let req = DocumentRequest::from_path(image).model("tesseract/default").timeout_secs(300.0);
    let resp = core::ocr(req).await.map_err(|e| {
        if e.message.contains("No such file") || e.message.contains("not found") || e.message.contains("cannot run") {
            format!("{e} (anonymizing needs Tesseract: `brew install tesseract` or `apt install tesseract-ocr`)")
        } else {
            e.to_string()
        }
    })?;
    Ok(resp
        .pages
        .into_iter()
        .flat_map(|p| p.words)
        .filter_map(|w| w.bbox.map(|bbox| PageWord { text: w.text, bbox }))
        .collect())
}

fn output_name(id: &str) -> String {
    if id.to_ascii_lowercase().ends_with(".pdf") {
        id.to_string()
    } else {
        format!("{id}.pdf")
    }
}

async fn anonymize_doc(
    root: &Path,
    out_root: &Path,
    doc: &DatasetDoc,
    kinds: &[String],
    terms: &[String],
    dpi: u32,
) -> Result<DocReport, String> {
    let path = root.join(&doc.id);
    let scratch = std::env::temp_dir().join(format!("puffinparse-anon-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let result = async {
        let (images, page_dpi): (Vec<(PathBuf, PageImage)>, u32) = match doc.kind.as_str() {
            "pdf" => {
                let mut v = Vec::new();
                for f in render_pdf(&path, &scratch, dpi).await? {
                    let data = std::fs::read(&f).map_err(|e| e.to_string())?;
                    let (width, height) = jpeg_size(&data).ok_or("pdftoppm wrote an unreadable JPEG")?;
                    let components = jpeg_components(&data);
                    v.push((f, PageImage::Jpeg { data, width, height, components }));
                }
                (v, dpi)
            }
            "jpg" | "jpeg" => {
                let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                let (width, height) = jpeg_size(&data).ok_or("unsupported JPEG (CMYK or damaged)")?;
                let components = jpeg_components(&data);
                (vec![(path.clone(), PageImage::Jpeg { data, width, height, components })], 150)
            }
            "png" => {
                let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                (vec![(path.clone(), png_image(&data)?)], 150)
            }
            other => return Err(format!(".{other} files are not supported; convert to PDF first")),
        };
        if images.is_empty() {
            return Err("no pages rendered".into());
        }
        let mut pages = Vec::new();
        let mut redactions: BTreeMap<String, usize> = BTreeMap::new();
        for (file, img) in images {
            let words = page_words(&file).await?;
            let hits = detect(&words, kinds, terms);
            let boxes = hits
                .iter()
                .map(|(i, kind)| {
                    *redactions.entry(kind.clone()).or_insert(0) += 1;
                    words[*i].bbox
                })
                .collect();
            pages.push((img, boxes));
        }
        let name = output_name(&doc.id);
        let dest = out_root.join(&name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Burn the boxes into the pixels: drawn over the original image, they would leave that image
        // (and a source JPEG's metadata) in the file for anyone who extracts it.
        let boxed = scratch.join("boxed.pdf");
        std::fs::write(&boxed, write_pdf(&pages, page_dpi)).map_err(|e| e.to_string())?;
        let flat_dir = scratch.join("flat");
        std::fs::create_dir_all(&flat_dir).map_err(|e| e.to_string())?;
        let mut flat = Vec::new();
        for f in render_pdf(&boxed, &flat_dir, page_dpi).await? {
            let data = std::fs::read(&f).map_err(|e| e.to_string())?;
            let (width, height) = jpeg_size(&data).ok_or("pdftoppm wrote an unreadable JPEG")?;
            let components = jpeg_components(&data);
            flat.push((PageImage::Jpeg { data, width, height, components }, Vec::new()));
        }
        if flat.len() != pages.len() {
            return Err("flattening the redacted pages lost a page".into());
        }
        std::fs::write(&dest, write_pdf(&flat, page_dpi))
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        Ok(DocReport { id: doc.id.clone(), output: Some(name), pages: pages.len(), redactions, skipped: None })
    }
    .await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

pub async fn anonymize(dataset_path: &Path, out: &Path, opts: &Options) -> Result<Report, String> {
    let ds = dataset::scan(dataset_path)?;
    let kinds: Vec<String> = if opts.kinds.is_empty() {
        DEFAULT_KINDS.iter().map(|s| s.to_string()).collect()
    } else {
        for k in &opts.kinds {
            if !ALL_KINDS.contains(&k.as_str()) {
                return Err(format!("unknown kind '{k}' (known: {})", ALL_KINDS.join(", ")));
            }
        }
        opts.kinds.clone()
    };
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let out_root = std::fs::canonicalize(out).map_err(|e| e.to_string())?;
    if out_root.starts_with(&ds.root) || ds.root.starts_with(&out_root) {
        return Err("the output folder must be outside the dataset folder (and not contain it)".into());
    }
    if std::fs::read_dir(&out_root).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("{} is not empty; pick a new folder", out_root.display()));
    }
    let dpi = opts.dpi.clamp(72, 400);
    let mut docs = Vec::new();
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for doc in &ds.docs {
        match anonymize_doc(&ds.root, &out_root, doc, &kinds, &opts.terms, dpi).await {
            Ok(r) => {
                for (k, n) in &r.redactions {
                    *totals.entry(k.clone()).or_insert(0) += n;
                }
                docs.push(r);
            }
            Err(e) => {
                // A missing engine fails every document the same way: stop at the first.
                if e.contains("needs Tesseract") || e.contains("install poppler") {
                    return Err(e);
                }
                docs.push(DocReport { id: doc.id.clone(), skipped: Some(e), ..Default::default() })
            }
        }
    }
    let report = Report {
        source: ds.root.to_string_lossy().into_owned(),
        output: out_root.to_string_lossy().into_owned(),
        kinds,
        terms: opts.terms.len(),
        documents: docs.len(),
        redacted_documents: docs.iter().filter(|d| d.output.is_some()).count(),
        redactions: totals,
        docs,
        notes: vec![
            "Detection is pattern-based (and term-based): look at a few redacted pages before treating the copy as \
             anonymous. The copies are image-only PDFs, so providers read them by OCR."
                .into(),
            "Ground-truth files were not copied: they would contain the same personal data.".into(),
        ],
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())? + "\n";
    std::fs::write(out_root.join(".puffinparse-redactions.json"), json).map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> PageWord {
        PageWord { text: text.into(), bbox: core::BBox { x0: 0.1, y0: 0.1, x1: 0.2, y1: 0.12 } }
    }

    fn kinds(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    fn hit_texts(words: &[PageWord], ks: &[&str], terms: &[&str]) -> Vec<(String, String)> {
        let terms: Vec<String> = terms.iter().map(|s| s.to_string()).collect();
        detect(words, &kinds(ks), &terms).into_iter().map(|(i, k)| (words[i].text.clone(), k)).collect()
    }

    #[test]
    fn detects_personal_data_but_not_ordinary_numbers() {
        let words: Vec<PageWord> =
            "Contact Jane Doe at jane.doe@acme.com or +1 (415) 555-0134. Card 4242 4242 4242 4242, \
             SSN 123-45-6789, IBAN DE89 3704 0044 0532 0130 00. Invoice total 1,234.56 on 2026-10-12, qty 12."
                .split_whitespace()
                .map(w)
                .collect();
        let hits = hit_texts(&words, DEFAULT_KINDS, &["jane doe"]);
        let by = |k: &str| hits.iter().filter(|(_, kind)| kind == k).map(|(t, _)| t.as_str()).collect::<Vec<_>>();
        assert_eq!(by("term"), ["Jane", "Doe"]);
        assert_eq!(by("email"), ["jane.doe@acme.com"]);
        assert_eq!(by("phone"), ["+1", "(415)", "555-0134."]);
        assert_eq!(by("card"), ["4242", "4242", "4242", "4242,"]);
        assert_eq!(by("ssn"), ["123-45-6789,"]);
        assert_eq!(by("iban"), ["DE89", "3704", "0044", "0532", "0130", "00."]);
        assert!(!hits.iter().any(|(t, _)| t == "1,234.56" || t == "12." || t.starts_with("2026")));
        // Dates and ids only when asked for.
        let hits = hit_texts(&words, &["date"], &[]);
        assert_eq!(hits, [("2026-10-12,".to_string(), "date".to_string())]);
        assert!(luhn("4242424242424242") && !luhn("4242424242424241"));
        // A card next to its expiry date or after a line number is still a card.
        for text in ["Card 4242 4242 4242 4242 12/28", "1 4242 4242 4242 4242"] {
            let words: Vec<PageWord> = text.split_whitespace().map(w).collect();
            let hits = hit_texts(&words, DEFAULT_KINDS, &[]);
            let cards = hits.iter().filter(|(_, k)| k == "card").count();
            assert!(cards >= 4, "{text}: {hits:?}");
        }
        assert!(is_id("INV-2026-00042") && !is_id("2026-00042") && !is_id("hello"));
    }

    #[test]
    fn writes_a_well_formed_image_pdf() {
        // A 2x1 grey PNG: IHDR + one IDAT with a zlib stream of two filtered rows.
        fn chunk(kind: &[u8], body: &[u8]) -> Vec<u8> {
            let mut c = (body.len() as u32).to_be_bytes().to_vec();
            c.extend_from_slice(kind);
            c.extend_from_slice(body);
            c.extend_from_slice(&[0, 0, 0, 0]); // CRC is not checked here
            c
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = 2u32.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &[0x78, 0x9c, 0x01, 0x02]));
        png.extend(chunk(b"IEND", &[]));
        let img = png_image(&png).unwrap();
        assert_eq!(img.size(), (2, 1));
        let mut rgba = png.clone();
        rgba[8 + 8 + 9] = 6; // colour type RGBA
        assert!(png_image(&rgba).err().unwrap().contains("transparency"));

        let boxes = vec![core::BBox { x0: 0.5, y0: 0.0, x1: 1.0, y1: 1.0 }];
        let pdf = write_pdf(&[(img, boxes)], 72);
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF-1.4"));
        assert!(text.contains("/Count 1"));
        assert!(text.contains("/Predictor 15 /Colors 1"));
        assert!(text.contains(" re f"));
        assert_eq!(puffinparse_server::pages::pdf_pages(&pdf), Some(1));
        // The xref offsets point at the objects.
        let tail = String::from_utf8_lossy(&pdf[pdf.len() - 40..]).into_owned();
        let xref_at: usize = tail.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
        assert!(pdf[xref_at..].starts_with(b"xref"));
        let table = String::from_utf8_lossy(&pdf[xref_at..]).into_owned();
        let first_obj: usize = table.lines().nth(3).unwrap()[..10].parse().unwrap();
        assert!(pdf[first_obj..].starts_with(b"1 0 obj"));
    }

    #[test]
    fn reads_jpeg_dimensions() {
        // SOI, an APP0 segment, then SOF0 with height 300, width 200, 3 components.
        let mut j = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00];
        j.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x01, 0x2c, 0x00, 0xc8, 0x03, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(jpeg_size(&j), Some((200, 300)));
        assert_eq!(jpeg_components(&j), 3);
        assert_eq!(jpeg_size(b"nope"), None);
    }

    #[tokio::test]
    async fn refuses_an_output_inside_the_dataset() {
        let dir = crate::eval::dataset::tempdir("anon");
        std::fs::write(dir.join("a.pdf"), crate::eval::dataset::tiny_pdf(1)).unwrap();
        let opts = Options { terms: vec![], kinds: vec![], dpi: 150 };
        let err = anonymize(&dir, &dir.join("redacted"), &opts).await.unwrap_err();
        assert!(err.contains("outside the dataset"));
        let err = anonymize(&dir, &dir.with_extension("out"), &Options { kinds: vec!["names".into()], ..opts }).await;
        assert!(err.unwrap_err().contains("unknown kind"));
    }

    /// End to end with the real engines; skipped where Tesseract or poppler is not installed.
    #[tokio::test]
    async fn redacts_a_real_pdf_with_local_engines() {
        let have = |cmd: &str| std::process::Command::new(cmd).arg("-v").output().is_ok();
        if !have("tesseract") || !have("pdftoppm") {
            eprintln!("skipped: tesseract or pdftoppm not installed");
            return;
        }
        let dir = crate::eval::dataset::tempdir("anon-real");
        let text = "BT /F1 28 Tf 60 700 Td (Write to jane.doe@example.com today) Tj ET \
                    BT /F1 28 Tf 60 640 Td (Invoice total 1234.56) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{text}\nendstream", text.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offs {
            pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes(),
        );
        std::fs::write(dir.join("letter.pdf"), pdf).unwrap();
        let out = dir.with_extension("redacted");
        let opts = Options { terms: vec!["invoice total".into()], kinds: vec![], dpi: 150 };
        let report = anonymize(&dir, &out, &opts).await.unwrap();
        assert_eq!(report.redacted_documents, 1, "{report:?}");
        assert_eq!(report.redactions.get("email"), Some(&1), "{report:?}");
        assert_eq!(report.redactions.get("term"), Some(&2), "{report:?}");
        let copy = std::fs::read(out.join("letter.pdf")).unwrap();
        assert_eq!(puffinparse_server::pages::pdf_pages(&copy), Some(1));
        // The copy is image-only: re-reading it finds neither the email nor the term.
        let words = page_words(&out.join("letter.pdf")).await.unwrap();
        let all: String = words.iter().map(|w| w.text.to_lowercase()).collect::<Vec<_>>().join(" ");
        assert!(!all.contains("jane.doe") && !all.contains("invoice"), "{all}");
        assert!(all.contains("today"), "{all}");
        // The boxes are in the pixels: no vector boxes, and OCR of the embedded image itself finds
        // nothing redacted.
        assert!(!String::from_utf8_lossy(&copy).contains(" re f"));
        let image = copy.windows(15).position(|w| w == b"/Subtype /Image").unwrap();
        let start = image + copy[image..].windows(7).position(|w| w == b"stream\n").unwrap() + 7;
        let end = start + copy[start..].windows(10).position(|w| w == b"\nendstream").unwrap();
        assert!(copy[start..].starts_with(&[0xff, 0xd8]), "the page image is a JPEG");
        let embedded = dir.with_extension("embedded.jpg");
        std::fs::write(&embedded, &copy[start..end]).unwrap();
        let words = page_words(&embedded).await.unwrap();
        let all: String = words.iter().map(|w| w.text.to_lowercase()).collect::<Vec<_>>().join(" ");
        assert!(!all.contains("jane.doe") && !all.contains("invoice"), "{all}");
        let manifest = std::fs::read_to_string(out.join(".puffinparse-redactions.json")).unwrap();
        assert!(!manifest.contains("jane"), "the report never repeats redacted text");
    }
}
