//! The version table (`versions.json`): per chat, the content hash of every
//! file the model has read in full or written, the text of that version (for
//! the "changed since seen" diff), and which lines Read returned at which
//! step since the last compaction.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Texts above this size are not kept (a stale edit then says "read it again").
const MAX_TEXT: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Versions {
    pub files: BTreeMap<String, Seen>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Seen {
    pub hash: String,
    pub size: u64,
    /// Every line of this version was returned by Read, or the model wrote it.
    pub full: bool,
    pub lines: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Line ranges Read returned since the last compaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shown: Vec<Shown>,
    /// The hash ("gone" when deleted) a turn's state delta already named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub told: Option<String>,
}

/// A file version as Read saw it.
pub struct Version<'a> {
    pub hash: &'a str,
    pub size: u64,
    pub lines: usize,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Shown {
    pub step: u64,
    pub from: usize,
    pub to: usize,
}

impl Versions {
    pub fn load(path: &Path) -> Versions {
        let mut v = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice::<Versions>(&b).unwrap_or_else(|e| {
                eprintln!(
                    "shadow: {} is not valid ({e}); starting an empty version table",
                    path.display()
                );
                Versions::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Versions::default(),
            Err(e) => {
                eprintln!(
                    "shadow: cannot read {} ({e}); starting an empty version table",
                    path.display()
                );
                Versions::default()
            }
        };
        v.path = path.to_path_buf();
        v
    }

    /// Writes the table when it changed (temp file + rename).
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(io::Error::other)?)?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn get(&self, path: &str) -> Option<&Seen> {
        self.files.get(path)
    }

    /// The step at which Read already returned lines `from..=to` of this
    /// version (an earlier step than `step`), if it did.
    pub fn unchanged_since(
        &self,
        path: &str,
        hash: &str,
        from: usize,
        to: usize,
        step: u64,
    ) -> Option<u64> {
        let seen = self.files.get(path).filter(|s| s.hash == hash)?;
        seen.shown
            .iter()
            .filter(|r| r.step < step && r.from <= from && to <= r.to)
            .map(|r| r.step)
            .min()
    }

    /// Read returned `ranges` (1-based, inclusive) of version `v` at `step`.
    /// `text` is the whole content when the caller has it.
    pub fn record_read(
        &mut self,
        path: &str,
        v: Version,
        ranges: &[(usize, usize)],
        step: u64,
        text: Option<String>,
    ) {
        let (size, lines) = (v.size, v.lines);
        let seen = self.files.entry(path.to_string()).or_default();
        if seen.hash != v.hash {
            *seen = Seen {
                hash: v.hash.to_string(),
                size,
                lines,
                ..Seen::default()
            };
        }
        for &(from, to) in ranges {
            seen.shown.push(Shown { step, from, to });
        }
        if !seen.full && covers(&seen.shown, lines) {
            seen.full = true;
        }
        if seen.full && seen.text.is_none() && size <= MAX_TEXT {
            seen.text = text;
        }
        self.dirty = true;
    }

    /// The model wrote this content (Write, Edit), or saw all of it in a diff.
    pub fn record_written(&mut self, path: &str, hash: &str, text: Option<String>) {
        let size = text.as_ref().map_or(0, |t| t.len() as u64);
        let lines = text.as_deref().map_or(0, line_count);
        let text = text.filter(|_| size <= MAX_TEXT);
        self.files.insert(
            path.to_string(),
            Seen {
                hash: hash.to_string(),
                size,
                full: true,
                lines,
                text,
                shown: Vec::new(),
                told: None,
            },
        );
        self.dirty = true;
    }

    /// A fade: what Read returned at `step` for `path` is no longer in view.
    pub fn forget(&mut self, path: &str, step: u64) {
        if let Some(seen) = self.files.get_mut(path) {
            seen.shown.retain(|r| r.step != step);
            self.dirty = true;
        }
    }

    /// The state delta's file lines: every file that differs from the
    /// version the model saw, named once per new version. Not "seen".
    pub fn changed_outside(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for (path, seen) in self.files.iter_mut() {
            let now = match std::fs::read(path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(_) => continue,
            };
            let tag = now.as_deref().map_or_else(|| "gone".to_string(), hash);
            if tag == seen.hash || seen.told.as_ref() == Some(&tag) {
                continue;
            }
            let text = now.as_deref().map(std::str::from_utf8);
            out.push(match (text, seen.text.as_deref()) {
                (None, _) => format!("{path} was deleted."),
                (Some(Ok(new)), Some(old)) => {
                    let (mut added, mut removed) = (0, 0);
                    let diff = similar::TextDiff::configure()
                        .timeout(std::time::Duration::from_millis(200))
                        .diff_lines(old, new);
                    for c in diff.iter_all_changes() {
                        match c.tag() {
                            similar::ChangeTag::Insert => added += 1,
                            similar::ChangeTag::Delete => removed += 1,
                            similar::ChangeTag::Equal => {}
                        }
                    }
                    format!("{path} was changed outside this chat: {added} lines added, {removed} removed.")
                }
                (Some(_), _) => format!(
                    "{path} was changed outside this chat (it is now {} bytes).",
                    now.as_ref().map_or(0, Vec::len)
                ),
            });
            seen.told = Some(tag);
            self.dirty = true;
        }
        out
    }

    /// Compaction: what Read returned is no longer in view.
    pub fn clear_shown(&mut self) {
        for seen in self.files.values_mut() {
            if !seen.shown.is_empty() {
                seen.shown.clear();
                self.dirty = true;
            }
        }
    }
}

/// The 1-based line ranges of `1..=lines` not in `shown`.
pub fn unseen(shown: &[Shown], lines: usize) -> Vec<(usize, usize)> {
    let mut r: Vec<(usize, usize)> = shown.iter().map(|s| (s.from, s.to)).collect();
    r.sort_unstable();
    let mut out = Vec::new();
    let mut next = 1;
    for (from, to) in r {
        if from > next {
            out.push((next, from - 1));
        }
        next = next.max(to + 1);
    }
    if next <= lines {
        out.push((next, lines));
    }
    out
}

fn covers(shown: &[Shown], lines: usize) -> bool {
    unseen(shown, lines).is_empty()
}

/// Lines of a text: a last line without "\n" counts; "" has none.
pub fn line_count(text: &str) -> usize {
    text.split_terminator('\n').count()
}

/// FNV-1a 64 over bytes, fed in pieces.
pub struct Hasher(u64);

impl Hasher {
    pub fn new() -> Hasher {
        Hasher(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

pub fn hash(bytes: &[u8]) -> String {
    let mut h = Hasher::new();
    h.update(bytes);
    h.hex()
}

#[cfg(test)]
#[path = "versions_test.rs"]
mod tests;
