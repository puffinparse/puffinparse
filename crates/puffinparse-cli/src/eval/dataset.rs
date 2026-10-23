//! A user's own dataset: a folder of documents, no manifest needed.
//!
//! Every supported file under the folder is one document; its id is its path relative to the
//! folder, with `/` separators, so ids stay readable and stable across runs. Hidden files and
//! folders are skipped, which also skips the `.puffinparse/` folder that runs write into.
//!
//! Optional ground truth: a reference transcript next to a document, `<file>.truth.md` (for
//! `invoices/a.pdf`: `invoices/a.pdf.truth.md`) or `<stem>.truth.md` (`invoices/a.truth.md`). When
//! present, `eval analyze` scores each model against it as well as against the other models.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File extensions treated as documents. Providers differ in what they accept; a model that
/// cannot read a type fails that document with the provider's own error.
pub const DOCUMENT_EXTENSIONS: &[&str] = &[
    "pdf", "png", "jpg", "jpeg", "tif", "tiff", "webp", "bmp", "gif", "heic", "docx", "doc", "pptx", "ppt", "xlsx",
    "xls", "html", "htm",
];

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "tif", "tiff", "webp", "bmp", "gif", "heic"];

/// Most documents a dataset may hold. The workflow is meant for a representative sample (tens to
/// a few hundred documents), not a whole archive.
pub const MAX_DOCUMENTS: usize = 2_000;

/// Folders this deep are not searched.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetDoc {
    /// Path relative to the dataset folder, `/`-separated.
    pub id: String,
    /// Lower-case extension (`pdf`, `png`, …).
    pub kind: String,
    pub bytes: u64,
    /// Page count: counted for PDFs, 1 for images, `None` when it cannot be told (office files, a
    /// PDF the counter cannot read). Cost estimates count an unknown document as one page.
    pub pages: Option<u32>,
    /// Reference transcript, relative to the dataset folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truth: Option<String>,
}

impl DatasetDoc {
    /// Pages a run is estimated to bill for this document, given an optional page selection.
    pub fn billed_pages(&self, selection: Option<&[(u32, Option<u32>)]>) -> u32 {
        let total = self.pages.unwrap_or(1).max(1);
        match selection {
            None => total,
            Some(ranges) => {
                let n: u32 = ranges
                    .iter()
                    .map(|(s, e)| {
                        let end = e.unwrap_or(total).min(total);
                        end.saturating_sub(s.saturating_sub(1)).min(total)
                    })
                    .sum();
                n.clamp(1, total)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    /// Canonical absolute path of the folder.
    pub root: PathBuf,
    pub docs: Vec<DatasetDoc>,
    /// Files that were not taken, with the reason (`unsupported type`, `empty`, …).
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

impl Dataset {
    pub fn total_pages(&self) -> u32 {
        // A corrupt page count must not overflow the sum.
        self.docs.iter().fold(0u32, |n, d| n.saturating_add(d.pages.unwrap_or(1).max(1)))
    }

    pub fn unknown_pages(&self) -> usize {
        self.docs.iter().filter(|d| d.pages.is_none()).count()
    }
}

/// Scan a folder (or a single document) into a [`Dataset`]. No network, no provider calls.
pub fn scan(path: &Path) -> Result<Dataset, String> {
    let root = std::fs::canonicalize(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let meta = std::fs::metadata(&root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    let mut docs = Vec::new();
    let mut skipped = Vec::new();
    if meta.is_file() {
        // A single document: its folder is the root, the document its only member.
        let parent = root.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match document(&parent, &name, &meta) {
            Ok(d) => docs.push(d),
            Err(reason) => return Err(format!("{}: {reason}", root.display())),
        }
        return Ok(Dataset { root: parent, docs, skipped });
    }
    walk(&root, &root, 0, &mut docs, &mut skipped)?;
    if docs.is_empty() {
        return Err(format!(
            "no documents found in {} (looked for: {})",
            root.display(),
            DOCUMENT_EXTENSIONS.join(", ")
        ));
    }
    docs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Dataset { root, docs, skipped })
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    docs: &mut Vec<DatasetDoc>,
    skipped: &mut Vec<Skipped>,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = relative_id(root, &path);
        // `file_type` does not follow symlinks; `metadata` does, so a linked document counts but a
        // linked folder is not walked (no cycles).
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth + 1 < MAX_DEPTH {
                walk(root, &path, depth + 1, docs, skipped)?;
            }
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        if is_truth_file(&name) {
            continue;
        }
        match document(root, &rel, &meta) {
            Ok(d) => {
                if docs.len() >= MAX_DOCUMENTS {
                    return Err(format!(
                        "more than {MAX_DOCUMENTS} documents under {}; point at a representative sample instead",
                        root.display()
                    ));
                }
                docs.push(d)
            }
            Err(reason) => skipped.push(Skipped { path: rel, reason }),
        }
    }
    Ok(())
}

fn relative_id(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

fn is_truth_file(name: &str) -> bool {
    name.ends_with(".truth.md")
}

fn extension(name: &str) -> Option<String> {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// One document's entry, or why the file is not one.
fn document(root: &Path, id: &str, meta: &std::fs::Metadata) -> Result<DatasetDoc, String> {
    let kind = extension(id).ok_or("no file extension")?;
    if !DOCUMENT_EXTENSIONS.contains(&kind.as_str()) {
        return Err(format!("unsupported type .{kind}"));
    }
    if meta.len() == 0 {
        return Err("empty file".into());
    }
    let path = root.join(id);
    let pages = if kind == "pdf" {
        std::fs::read(&path).ok().and_then(|bytes| puffinparse_server::pages::pdf_pages_estimate(&bytes))
    } else if IMAGE_EXTENSIONS.contains(&kind.as_str()) {
        Some(1)
    } else {
        None
    };
    let truth = truth_for(root, id);
    Ok(DatasetDoc { id: id.to_string(), kind, bytes: meta.len(), pages, truth })
}

fn truth_for(root: &Path, id: &str) -> Option<String> {
    let with_ext = format!("{id}.truth.md");
    if root.join(&with_ext).is_file() {
        return Some(with_ext);
    }
    let stem = match id.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.truth.md"),
        None => return None,
    };
    root.join(&stem).is_file().then_some(stem)
}

#[cfg(test)]
pub(crate) use tests::{tempdir, tiny_pdf};

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tiny_pdf(pages: u32) -> Vec<u8> {
        let mut s = String::from("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        s.push_str(&format!("2 0 obj << /Type /Pages /Count {pages} /Kids [] >> endobj\n"));
        for i in 0..pages {
            s.push_str(&format!("{} 0 obj << /Type /Page /Parent 2 0 R >> endobj\n", 3 + i));
        }
        s.push_str("%%EOF\n");
        s.into_bytes()
    }

    #[test]
    fn scans_folders_skips_hidden_and_truth_files() {
        let dir = tempdir("scan");
        std::fs::create_dir_all(dir.join("invoices")).unwrap();
        std::fs::create_dir_all(dir.join(".puffinparse/runs")).unwrap();
        std::fs::write(dir.join("invoices/a.pdf"), tiny_pdf(3)).unwrap();
        std::fs::write(dir.join("invoices/a.pdf.truth.md"), "# A").unwrap();
        std::fs::write(dir.join("scan.PNG"), b"\x89PNG\r\n\x1a\nxx").unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        std::fs::write(dir.join("empty.pdf"), b"").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"x").unwrap();
        std::fs::write(dir.join(".puffinparse/runs/x.pdf"), tiny_pdf(1)).unwrap();
        std::fs::write(dir.join("memo.docx"), b"PK..").unwrap();

        let ds = scan(&dir).unwrap();
        let ids: Vec<_> = ds.docs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["invoices/a.pdf", "memo.docx", "scan.PNG"]);
        assert_eq!(ds.docs[0].pages, Some(3));
        assert_eq!(ds.docs[0].truth.as_deref(), Some("invoices/a.pdf.truth.md"));
        assert_eq!(ds.docs[1].pages, None);
        assert_eq!(ds.docs[2].pages, Some(1));
        assert_eq!(ds.docs[2].kind, "png");
        assert_eq!(ds.total_pages(), 5);
        assert_eq!(ds.unknown_pages(), 1);
        let reasons: Vec<_> = ds.skipped.iter().map(|s| (s.path.as_str(), s.reason.as_str())).collect();
        assert!(reasons.contains(&("notes.txt", "unsupported type .txt")));
        assert!(reasons.contains(&("empty.pdf", "empty file")));
    }

    #[test]
    fn a_single_file_is_a_dataset_of_one() {
        let dir = tempdir("single");
        std::fs::write(dir.join("one.pdf"), tiny_pdf(2)).unwrap();
        std::fs::write(dir.join("one.truth.md"), "x").unwrap();
        let ds = scan(&dir.join("one.pdf")).unwrap();
        assert_eq!(ds.docs.len(), 1);
        assert_eq!(ds.docs[0].truth.as_deref(), Some("one.truth.md"));
        assert!(scan(&dir.join("missing.pdf")).is_err());
    }

    #[test]
    fn billed_pages_follow_the_selection() {
        let d = DatasetDoc { id: "a.pdf".into(), kind: "pdf".into(), bytes: 1, pages: Some(10), truth: None };
        assert_eq!(d.billed_pages(None), 10);
        let sel = puffinparse_core::util::parse_page_ranges("1-3").unwrap();
        assert_eq!(d.billed_pages(Some(&sel)), 3);
        let sel = puffinparse_core::util::parse_page_ranges("8-").unwrap();
        assert_eq!(d.billed_pages(Some(&sel)), 3);
        let sel = puffinparse_core::util::parse_page_ranges("1-50").unwrap();
        assert_eq!(d.billed_pages(Some(&sel)), 10);
        let unknown = DatasetDoc { pages: None, ..d };
        assert_eq!(unknown.billed_pages(Some(&puffinparse_core::util::parse_page_ranges("1-3").unwrap())), 1);
    }

    pub(crate) fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pp-eval-{tag}-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }
}
