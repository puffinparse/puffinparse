//! What the playground needs to know about an upload before any provider sees it: its type (by
//! magic bytes, never by name) and, for a PDF, how many pages it has.
//!
//! The page count gates the free tier's quota, so it must not be fooled by a modern PDF that keeps
//! its page objects inside compressed object streams. [`pdf_pages`] counts `/Type /Page` objects in
//! the file and in every Flate-compressed stream (bounded, so a decompression bomb costs little),
//! and also reads the page tree's `/Count`. It returns the larger of the two, or `None` when it
//! finds neither.

use std::io::Read;

/// The accepted document types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Png,
    Jpeg,
}

impl Kind {
    /// The filename the gateway sends to providers (never the user's own).
    pub fn filename(self) -> &'static str {
        match self {
            Kind::Pdf => "document.pdf",
            Kind::Png => "document.png",
            Kind::Jpeg => "document.jpg",
        }
    }
}

pub fn sniff(data: &[u8]) -> Option<Kind> {
    if data.starts_with(b"%PDF-") {
        Some(Kind::Pdf)
    } else if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some(Kind::Png)
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Kind::Jpeg)
    } else {
        None
    }
}

/// Total bytes inflated across all object streams of one document. Page-tree objects are small;
/// with at most [`MAX_CONCURRENT_SCANS`] scans at once this bounds the memory they use.
const MAX_INFLATED: u64 = 16 * 1024 * 1024;
/// PDF scans running at once in the gateway (each on a blocking thread).
pub const MAX_CONCURRENT_SCANS: usize = 4;
/// How far back from a keyword to look for the `<<` that opens its dictionary. Bounds the work
/// per keyword, so a file full of streams stays linear.
const DICT_REACH: usize = 4096;

/// Start of the dictionary that encloses position `at` (its `<<`), looking back `DICT_REACH`.
fn dict_start(data: &[u8], at: usize) -> usize {
    let from = at.saturating_sub(DICT_REACH);
    data[from..at].windows(2).rposition(|w| w == b"<<").map_or(from, |p| from + p)
}

/// The number of pages in a PDF, or `None` when it cannot be told.
pub fn pdf_pages(data: &[u8]) -> Option<u32> {
    let mut objects = count_page_objects(data);
    let mut tree = max_count(data);
    let mut budget = MAX_INFLATED;
    for stream in object_streams(data) {
        if budget == 0 {
            break;
        }
        let mut out = Vec::new();
        let mut z = flate2::read::ZlibDecoder::new(stream).take(budget);
        if z.read_to_end(&mut out).is_err() && out.is_empty() {
            continue;
        }
        budget = budget.saturating_sub(out.len() as u64);
        objects += count_page_objects(&out);
        tree = tree.max(max_count(&out));
    }
    let n = objects.max(tree);
    (n > 0).then_some(n)
}

fn is_delim(b: Option<&u8>) -> bool {
    !matches!(b, Some(c) if c.is_ascii_alphanumeric())
}

fn skip_ws(data: &[u8], mut i: usize) -> usize {
    while i < data.len() && matches!(data[i], b' ' | b'\r' | b'\n' | b'\t' | b'\x0c' | b'\0') {
        i += 1;
    }
    i
}

/// `/Type /Page` (not `/Pages`) occurrences.
fn count_page_objects(data: &[u8]) -> u32 {
    let mut n = 0u32;
    for i in find_all(data, b"/Type") {
        let j = skip_ws(data, i + 5);
        if data[j..].starts_with(b"/Page") && is_delim(data.get(j + 5)) {
            n = n.saturating_add(1);
        }
    }
    n
}

/// The largest `/Count N` that belongs to a `/Type /Pages` dictionary (the root holds the total).
fn max_count(data: &[u8]) -> u32 {
    let mut best = 0u32;
    for i in find_all(data, b"/Type") {
        let j = skip_ws(data, i + 5);
        if !(data[j..].starts_with(b"/Pages") && is_delim(data.get(j + 6))) {
            continue;
        }
        // The dictionary around this `/Type /Pages`: back to its `<<`, forward to its `>>`.
        let start = dict_start(data, i);
        let end = data[j..(j + DICT_REACH).min(data.len())]
            .windows(2)
            .position(|w| w == b">>")
            .map_or((j + DICT_REACH).min(data.len()), |p| j + p);
        let dict = &data[start..end];
        if let Some(k) = find_all(dict, b"/Count").next() {
            let mut p = skip_ws(dict, k + 6);
            let mut v: u64 = 0;
            while p < dict.len() && dict[p].is_ascii_digit() && v < u64::from(u32::MAX) {
                v = v * 10 + u64::from(dict[p] - b'0');
                p += 1;
            }
            best = best.max(u32::try_from(v).unwrap_or(u32::MAX));
        }
    }
    best
}

fn find_all<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    hay.windows(needle.len()).enumerate().filter(move |(_, w)| *w == needle).map(|(i, _)| i)
}

/// The raw bytes of every compressed object stream: a stream whose dictionary names `/ObjStm`
/// and `/FlateDecode` (`/Fl` abbreviation too). Page objects live either in plain objects or in
/// object streams, so content and image streams (most of a file's bytes) are never inflated.
fn object_streams(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = data[from..].windows(6).position(|w| w == b"stream") {
        let kw = from + p;
        from = kw + 6;
        // `endstream` also contains "stream"; skip it.
        if kw >= 3 && &data[kw - 3..kw] == b"end" {
            continue;
        }
        let dict = &data[dict_start(data, kw)..kw];
        if find_all(dict, b"/ObjStm").next().is_none() {
            continue;
        }
        if !(find_all(dict, b"/FlateDecode").next().is_some()
            || find_all(dict, b"/Fl").any(|i| is_delim(dict.get(i + 3))))
        {
            continue;
        }
        let mut s = kw + 6;
        if data.get(s) == Some(&b'\r') {
            s += 1;
        }
        if data.get(s) == Some(&b'\n') {
            s += 1;
        }
        let end = data[s..].windows(9).position(|w| w == b"endstream").map_or(data.len(), |e| s + e);
        out.push(&data[s..end]);
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn deflate(text: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(text).unwrap();
        z.finish().unwrap()
    }

    #[test]
    fn sniffs_by_magic_bytes() {
        assert_eq!(sniff(b"%PDF-1.7\n"), Some(Kind::Pdf));
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]), Some(Kind::Png));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(Kind::Jpeg));
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(sniff(b"%PD"), None);
    }

    #[test]
    fn counts_plain_page_objects() {
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Pages /Kids [2 0 R 3 0 R] /Count 2 >> endobj\n\
                    2 0 obj << /Type /Page /Parent 1 0 R >> endobj\n3 0 obj <</Type/Page/Parent 1 0 R>> endobj\n";
        assert_eq!(pdf_pages(pdf), Some(2));
    }

    #[test]
    fn counts_pages_hidden_in_compressed_object_streams() {
        let objs = deflate(b"<< /Type /Pages /Kids [4 0 R 5 0 R 6 0 R] /Count 3 >> << /Type /Page >> << /Type /Page >> << /Type /Page >>");
        let mut pdf =
            b"%PDF-1.7\n7 0 obj << /Type /ObjStm /N 4 /First 20 /Filter /FlateDecode /Length 99 >>\nstream\r\n"
                .to_vec();
        pdf.extend_from_slice(&objs);
        pdf.extend_from_slice(b"\r\nendstream\nendobj\n");
        // Nothing countable outside the stream.
        assert_eq!(count_page_objects(&pdf), 0);
        assert_eq!(pdf_pages(&pdf), Some(3));
    }

    #[test]
    fn the_page_tree_count_wins_when_page_objects_are_unreadable() {
        let pdf = b"%PDF-1.7\n1 0 obj << /Type /Pages /Count 40 /Kids [] >> endobj\n2 0 obj << /Type /Page >> endobj";
        assert_eq!(pdf_pages(pdf), Some(40));
    }

    #[test]
    fn unknown_when_nothing_is_found() {
        assert_eq!(pdf_pages(b"%PDF-1.7\n%%EOF"), None);
        // A broken stream is skipped, not fatal.
        assert_eq!(pdf_pages(b"%PDF-1.7\n<< /Type /ObjStm /Filter /FlateDecode >>\nstream\nnot zlib\nendstream"), None);
    }

    #[test]
    fn a_decompression_bomb_is_bounded() {
        let bomb = deflate(&vec![b' '; 80 * 1024 * 1024]);
        let mut pdf = b"%PDF-1.7\n<< /Type /ObjStm /Filter /FlateDecode >>\nstream\n".to_vec();
        pdf.extend_from_slice(&bomb);
        pdf.extend_from_slice(b"\nendstream\n<< /Type /Page >>");
        let t = std::time::Instant::now();
        assert_eq!(pdf_pages(&pdf), Some(1));
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn content_streams_are_not_inflated() {
        // A page dictionary inside a content (non-object) stream is not a page object.
        let content = deflate(b"<< /Type /Page >> << /Type /Page >>");
        let mut pdf =
            b"%PDF-1.7\n1 0 obj << /Type /Page >> endobj\n<< /Filter /FlateDecode /Length 9 >>\nstream\n".to_vec();
        pdf.extend_from_slice(&content);
        pdf.extend_from_slice(b"\nendstream\n");
        assert_eq!(pdf_pages(&pdf), Some(1));
    }
}
