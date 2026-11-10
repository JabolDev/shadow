//! Read: numbered lines of a text file, or an image block.

use super::{Cx, Out, opt_num, req_str};
use crate::versions::{Hasher, Version};
use base64::Engine;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
/// Base64 of this many bytes is 5 MB, the usual image limit of model APIs.
const MAX_IMAGE: u64 = 3_932_160;
/// Up to this size the text is kept for the version table.
const KEEP_TEXT: u64 = 4 * 1024 * 1024;

pub fn read(cx: &Cx, input: &Value) -> Out {
    run(cx, input).unwrap_or_else(Out::error)
}

fn run(cx: &Cx, input: &Value) -> Result<Out, String> {
    let path = cx.path(req_str(input, "file_path")?);
    let offset = opt_num(input, "offset")?.unwrap_or(1).max(1) as usize;
    let limit = opt_num(input, "limit")?.map_or(DEFAULT_LIMIT, |l| l.max(1) as usize);
    let shown = path.display().to_string();
    let io_err = |e: io::Error| format!("Cannot read {shown}: {e}");
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing(&path)),
        Err(e) => return Err(io_err(e)),
    };
    if meta.is_dir() {
        return Err(format!(
            "{shown} is a folder, not a file. Use Glob to list its files."
        ));
    }
    let mut r = BufReader::with_capacity(64 * 1024, File::open(&path).map_err(io_err)?);
    let head = r.fill_buf().map_err(io_err)?.to_vec();
    if let Some(mime) = image_type(&head) {
        return image(cx, &path, mime, meta.len());
    }
    if head[..head.len().min(8192)].contains(&0) {
        return Err(format!(
            "{shown} is a binary file ({}, {} bytes). Read shows text and PNG, JPEG, GIF or WebP images only.",
            kind(&head),
            meta.len()
        ));
    }

    let keep = meta.len() <= KEEP_TEXT;
    let mut all = Vec::new();
    let mut hasher = Hasher::new();
    let mut total = 0usize;
    let mut lines = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf).map_err(io_err)? == 0 {
            break;
        }
        hasher.update(&buf);
        if keep {
            all.extend_from_slice(&buf);
        }
        total += 1;
        if total >= offset && total - offset < limit {
            lines.push(display_line(&buf));
        }
    }
    let hash = hasher.hex();
    let key = path.to_string_lossy().into_owned();
    let text = if keep {
        String::from_utf8(all).ok()
    } else {
        None
    };
    let step = cx.step;
    if total == 0 {
        cx.turn.versions().record_read(
            &key,
            Version {
                hash: &hash,
                size: meta.len(),
                lines: 0,
            },
            &[],
            step,
            text,
        );
        return Ok(Out::text(format!("{shown} is empty.")));
    }
    if offset > total {
        return Ok(Out::text(format!(
            "{shown} has {total} lines; offset {offset} is past its end."
        )));
    }
    let (a, b) = (offset, offset + lines.len() - 1);
    if let Some(n) = cx.turn.versions().unchanged_since(&key, &hash, a, b, step) {
        return Ok(Out::text(format!(
            "Unchanged since step {n} (lines {a}–{b} are above)."
        )));
    }
    let body: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{l}", a + i))
        .collect();
    let mut bounded = cx
        .bound(&body.join("\n"))
        .map_err(|e| format!("Cannot write the overflow file: {e}"))?;
    let ranges = match bounded.overflow {
        None => vec![(a, b)],
        Some(_) => {
            let mut r = Vec::new();
            if bounded.head > 0 {
                r.push((a, a + bounded.head - 1));
            }
            if bounded.tail > 0 {
                r.push((b + 1 - bounded.tail, b));
            }
            r
        }
    };
    cx.turn.versions().record_read(
        &key,
        Version {
            hash: &hash,
            size: meta.len(),
            lines: total,
        },
        &ranges,
        step,
        text,
    );
    if a > 1 || b < total {
        bounded.text.push_str(&format!(
            "\n\n(Lines {a}–{b} of {total}. Use offset and limit to read other lines.)"
        ));
    }
    Ok(Out::bounded(bounded, false))
}

/// One line as shown: no line ending, invalid UTF-8 replaced, cut at 2000 characters.
fn display_line(raw: &[u8]) -> String {
    let mut end = raw.len();
    while end > 0 && matches!(raw[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let line = String::from_utf8_lossy(&raw[..end]);
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!(
            "{}… [line cut at {MAX_LINE_CHARS} characters]",
            &line[..cut]
        ),
        None => line.into_owned(),
    }
}

fn image(cx: &Cx, path: &Path, mime: &str, size: u64) -> Result<Out, String> {
    if size > MAX_IMAGE {
        return Err(format!(
            "{} is a {mime} image of {size} bytes; images over {MAX_IMAGE} bytes cannot be sent.",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let key = path.to_string_lossy();
    cx.turn.versions().record_read(
        &key,
        Version {
            hash: &crate::versions::hash(&bytes),
            size,
            lines: 0,
        },
        &[],
        cx.step,
        None,
    );
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(Out {
        content: vec![
            json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": data } }),
        ],
        ..Out::default()
    })
}

/// The image type told by the first bytes.
fn image_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A name for other binary content, told by its first bytes.
fn kind(b: &[u8]) -> &'static str {
    const KINDS: [(&[u8], &str); 8] = [
        (b"%PDF-", "PDF document"),
        (b"PK\x03\x04", "ZIP archive"),
        (b"\x1f\x8b", "gzip archive"),
        (b"\x7fELF", "ELF executable"),
        (b"SQLite format 3\0", "SQLite database"),
        (b"BM", "BMP image"),
        (b"\0asm", "WebAssembly module"),
        (b"\xca\xfe\xba\xbe", "Java class or Mach-O file"),
    ];
    KINDS
        .iter()
        .find(|(magic, _)| b.starts_with(magic))
        .map_or("binary data", |(_, k)| k)
}

/// "File does not exist", with up to 5 similar names from its folder.
fn missing(path: &Path) -> String {
    let mut msg = format!("File does not exist: {}.", path.display());
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return msg;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        msg.push_str(&format!(
            " Its folder {} does not exist either.",
            dir.display()
        ));
        return msg;
    };
    let want = name.to_string_lossy().to_lowercase();
    let mut scored: Vec<(f32, String)> = entries
        .flatten()
        .take(10_000)
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let r = similar::TextDiff::from_chars(want.as_str(), n.to_lowercase().as_str()).ratio();
            (r >= 0.5).then_some((r, n))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    if !scored.is_empty() {
        let names: Vec<String> = scored
            .iter()
            .take(5)
            .map(|(_, n)| dir.join(n).display().to_string())
            .collect();
        msg.push_str(&format!(
            " Similar names in that folder: {}",
            names.join(", ")
        ));
    }
    msg
}
