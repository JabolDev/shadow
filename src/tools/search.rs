//! Grep (ripgrep's search crates) and Glob. Both walk with the same rules:
//! `.gitignore` respected, hidden files included, `.git` skipped, names in
//! sorted order.

use super::{Cx, Out, bound_file, opt_bool, opt_num, opt_str, req_str};
use crate::turn::{STOPPED, Turn};
use globset::GlobBuilder;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_GLOB: usize = 100;

fn walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .filter_entry(|e| e.file_name() != ".git")
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
}

pub fn grep(cx: &Cx, input: &Value) -> Out {
    run_grep(cx, input).unwrap_or_else(Out::error)
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Files,
    Content,
    Count,
}

fn run_grep(cx: &Cx, input: &Value) -> Result<Out, String> {
    let pattern = req_str(input, "pattern")?;
    let target = opt_str(input, "path")?.map_or_else(|| cx.turn.cwd.clone(), |p| cx.path(p));
    let mode = match opt_str(input, "output_mode")?.unwrap_or("files_with_matches") {
        "files_with_matches" => Mode::Files,
        "content" => Mode::Content,
        "count" => Mode::Count,
        other => {
            return Err(format!(
                "output_mode must be files_with_matches, content or count (got {other:?})."
            ));
        }
    };
    let multiline = opt_bool(input, "multiline")?;
    let around = opt_num(input, "-C")?;
    let context = |k: &str| -> Result<usize, String> {
        Ok(opt_num(input, k)?.or(around).unwrap_or(0).max(0) as usize)
    };
    let (after, before) = (context("-A")?, context("-B")?);
    let limit = opt_num(input, "head_limit")?
        .filter(|n| *n > 0)
        .map_or(usize::MAX, |n| n as usize);
    let glob = match opt_str(input, "glob")? {
        Some(g) => Some((
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("Invalid glob: {e}"))?
                .compile_matcher(),
            g.contains('/'),
        )),
        None => None,
    };
    let mut mb = RegexMatcherBuilder::new();
    mb.case_insensitive(opt_bool(input, "-i")?);
    if multiline {
        mb.multi_line(true).dot_matches_new_line(true);
    } else {
        mb.line_terminator(Some(b'\n'));
    }
    let matcher = mb
        .build(pattern)
        .map_err(|e| format!("Invalid pattern: {e}"))?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .multi_line(multiline)
        .before_context(before)
        .after_context(after)
        .build();
    let meta = std::fs::metadata(&target)
        .map_err(|_| format!("Path does not exist: {}", target.display()))?;

    let files: Box<dyn Iterator<Item = Result<PathBuf, String>>> = if meta.is_file() {
        Box::new(std::iter::once(Ok(target.clone())))
    } else {
        Box::new(walker(&target).filter_map(|e| match e {
            Ok(e) if e.file_type().is_some_and(|t| t.is_file()) => Some(Ok(e.into_path())),
            Ok(_) => None,
            Err(err) => Some(Err(err.to_string())),
        }))
    };
    let mut c = Collect {
        mode,
        numbers: opt_bool(input, "-n")?,
        room: limit,
        shown: 0,
        more: 0,
        out: String::new(),
        spill: None,
        spill_at: (4 * cx.cap()).max(1 << 20),
        spill_path: cx.overflow_path().with_extension("part"),
        err: None,
        path: String::new(),
        count: 0,
    };
    let (mut files_found, mut failed) = (0usize, 0usize);
    for file in files {
        if cx.turn.stopped() {
            return Err(STOPPED.into());
        }
        let Ok(file) = file else {
            failed += 1;
            continue;
        };
        if let Some((m, full)) = &glob {
            let rel = file.strip_prefix(&target).unwrap_or(&file);
            let ok = if *full {
                m.is_match(rel)
            } else {
                file.file_name().is_some_and(|n| m.is_match(n))
            };
            if !ok {
                continue;
            }
        }
        c.path = file.display().to_string();
        c.count = 0;
        let found = File::open(&file).and_then(|f| {
            let reader = StopRead {
                file: f,
                turn: &cx.turn,
            };
            searcher.search_reader(&matcher, reader, &mut c)
        });
        if found.is_err() {
            failed += 1;
            continue;
        }
        if c.count > 0 {
            files_found += 1;
            match mode {
                Mode::Files => c.push(c.path.clone()),
                Mode::Count => c.push(format!("{}:{}", c.path, c.count)),
                Mode::Content => {}
            }
        }
    }
    if cx.turn.stopped() {
        return Err(STOPPED.into());
    }
    if let Some(e) = c.err.take() {
        return Err(format!("Cannot write the overflow file: {e}"));
    }
    let head = match (c.shown + c.more, mode) {
        (0, Mode::Files) => "No files found".to_string(),
        (0, _) => "No matches found".to_string(),
        (_, Mode::Files) => format!(
            "Found {files_found} file{}",
            if files_found == 1 { "" } else { "s" }
        ),
        _ => String::new(),
    };
    let mut foot = String::new();
    if c.more > 0 {
        foot.push_str(&format!("({} more not shown)\n", c.more));
    }
    if failed > 0 {
        foot.push_str(&format!("({failed} paths could not be read)\n"));
    }
    let io_err = |e: io::Error| format!("Cannot write the overflow file: {e}");
    let Some((part, f)) = c.spill.take() else {
        let parts = [head.as_str(), c.out.trim_end(), foot.trim_end()];
        let text = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Out::bounded(cx.bound(&text).map_err(io_err)?, false));
    };
    // Too long to keep in memory: the full text is assembled in the overflow file.
    let path = cx.overflow_path();
    let assemble = || -> io::Result<()> {
        f.into_inner().map_err(|e| e.into_error())?;
        let mut out = BufWriter::new(File::create(&path)?);
        if !head.is_empty() {
            writeln!(out, "{head}")?;
        }
        io::copy(&mut File::open(&part)?, &mut out)?;
        out.write_all(foot.trim_end().as_bytes())?;
        out.flush()?;
        std::fs::remove_file(&part)
    };
    assemble().map_err(io_err)?;
    Ok(Out::bounded(
        bound_file(&path, cx.cap(), &path).map_err(io_err)?,
        false,
    ))
}

/// A file that reads nothing more once the turn is stopped, so a long
/// search of one file ends with the turn.
struct StopRead<'a> {
    file: File,
    turn: &'a Turn,
}

impl Read for StopRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.turn.stopped() {
            return Err(io::Error::other(STOPPED));
        }
        self.file.read(buf)
    }
}

/// Collects output lines within the entry limit; past a size they are
/// streamed to a file instead of kept in memory.
struct Collect {
    mode: Mode,
    numbers: bool,
    room: usize,
    shown: usize,
    more: usize,
    out: String,
    spill: Option<(PathBuf, BufWriter<File>)>,
    spill_at: usize,
    spill_path: PathBuf,
    err: Option<io::Error>,
    path: String,
    count: usize,
}

impl Collect {
    fn push(&mut self, line: String) {
        if self.shown >= self.room {
            self.more += 1;
            return;
        }
        self.shown += 1;
        if let Err(e) = self.keep(&line) {
            self.err.get_or_insert(e);
        }
    }

    fn keep(&mut self, line: &str) -> io::Result<()> {
        if let Some((_, f)) = &mut self.spill {
            return writeln!(f, "{line}");
        }
        self.out.push_str(line);
        self.out.push('\n');
        if self.out.len() > self.spill_at {
            if let Some(dir) = self.spill_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = BufWriter::new(File::create(&self.spill_path)?);
            f.write_all(self.out.as_bytes())?;
            self.out = String::new();
            self.spill = Some((self.spill_path.clone(), f));
        }
        Ok(())
    }

    fn add(&mut self, first: Option<u64>, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        for (i, line) in text.trim_end_matches(['\n', '\r']).split('\n').enumerate() {
            let line = line.trim_end_matches('\r');
            let out = match (self.numbers, first) {
                (true, Some(n)) => format!("{}:{}:{line}", self.path, n + i as u64),
                _ => format!("{}:{line}", self.path),
            };
            self.push(out);
        }
    }
}

impl Sink for Collect {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.count += 1;
        match self.mode {
            Mode::Files => return Ok(false),
            Mode::Count => {}
            Mode::Content => self.add(m.line_number(), m.bytes()),
        }
        Ok(self.err.is_none())
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.mode == Mode::Content {
            self.add(c.line_number(), c.bytes());
        }
        Ok(self.err.is_none())
    }
}

pub fn glob(cx: &Cx, input: &Value) -> Out {
    run_glob(cx, input).unwrap_or_else(Out::error)
}

fn run_glob(cx: &Cx, input: &Value) -> Result<Out, String> {
    let pattern = req_str(input, "pattern")?;
    let base = opt_str(input, "path")?.map_or_else(|| cx.turn.cwd.clone(), |p| cx.path(p));
    let (root, pat) = if Path::new(pattern).is_absolute() {
        match Path::new(pattern).strip_prefix(&base) {
            Ok(rel) => (base.clone(), rel.to_string_lossy().into_owned()),
            Err(_) => split_absolute(pattern),
        }
    } else {
        (base, pattern.to_string())
    };
    if !root.is_dir() {
        return Err(format!("Folder does not exist: {}", root.display()));
    }
    let matcher = GlobBuilder::new(&pat)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("Invalid pattern: {e}"))?
        .compile_matcher();
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let (mut total, mut failed) = (0usize, 0usize);
    for entry in walker(&root) {
        if cx.turn.stopped() {
            return Err(STOPPED.into());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if !matcher.is_match(rel) {
            continue;
        }
        total += 1;
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((mtime, entry.into_path()));
        if found.len() >= 10 * MAX_GLOB {
            newest(&mut found);
        }
    }
    newest(&mut found);
    let mut text = if found.is_empty() {
        "No files found".to_string()
    } else {
        found
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if total > found.len() {
        text.push_str(&format!("\n({} more not shown)", total - found.len()));
    }
    if failed > 0 {
        text.push_str(&format!("\n({failed} paths could not be read)"));
    }
    let bounded = cx
        .bound(&text)
        .map_err(|e| format!("Cannot write the overflow file: {e}"))?;
    Ok(Out::bounded(bounded, false))
}

/// Keeps the newest MAX_GLOB, newest first (then by path).
fn newest(found: &mut Vec<(SystemTime, PathBuf)>) {
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.truncate(MAX_GLOB);
}

/// An absolute pattern as (the folder before its first wildcard, the rest).
fn split_absolute(pattern: &str) -> (PathBuf, String) {
    let parts: Vec<&str> = pattern.split('/').collect();
    let first_glob = parts
        .iter()
        .position(|p| p.contains(['*', '?', '[', '{']))
        .unwrap_or(parts.len() - 1);
    let root = parts[..first_glob].join("/");
    (
        PathBuf::from(if root.is_empty() { "/" } else { &root }),
        parts[first_glob..].join("/"),
    )
}
