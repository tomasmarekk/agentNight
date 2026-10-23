//! Cheap, incremental access to agent session logs.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meta {
    /// Modification time in epoch milliseconds.
    pub mtime: i64,
    pub len: u64,
}

pub fn meta(path: &Path) -> Option<Meta> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    Some(Meta {
        mtime: m.modified().ok().map(epoch_ms).unwrap_or(0),
        len: m.len(),
    })
}

pub fn epoch_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `AGENTNIGHT_HOME` replaces the home directory for detection (used for testing).
fn test_home() -> Option<PathBuf> {
    std::env::var_os("AGENTNIGHT_HOME").map(PathBuf::from)
}

/// The user's home directory as seen by the detectors.
pub fn home_dir() -> Option<PathBuf> {
    test_home().or_else(dirs::home_dir)
}

pub fn now_ms() -> i64 {
    epoch_ms(SystemTime::now())
}

/// Where to look for session files of one harness.
#[derive(Clone, Copy, Debug)]
pub struct LogSpec {
    pub base: Base,
    pub rel: &'static str,
    /// Directory levels below `rel` that may contain matching files.
    pub depth: u8,
    /// File name must end with one of these (empty = any file).
    pub suffixes: &'static [&'static str],
    /// If set, files must sit directly in a directory with this name.
    pub parent: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
pub enum Base {
    Home,
    /// `%APPDATA%` / `~/Library/Application Support` / `~/.config`.
    Config,
    /// `%APPDATA%` / `~/Library/Application Support` / `~/.local/share`.
    Data,
}

impl LogSpec {
    pub fn resolve(&self) -> Option<Root> {
        let base = match (self.base, test_home()) {
            (Base::Home, Some(h)) => Some(h),
            (Base::Config, Some(h)) => Some(h.join(".config")),
            (Base::Data, Some(h)) => Some(h.join(".local/share")),
            (Base::Home, None) => dirs::home_dir(),
            (Base::Config, None) => dirs::config_dir(),
            (Base::Data, None) => dirs::data_dir(),
        }?;
        Some(Root {
            dir: base.join(self.rel),
            depth: self.depth,
            suffixes: self.suffixes,
            parent: self.parent,
        })
    }
}

/// A resolved directory to search for session files.
#[derive(Clone, Debug)]
pub struct Root {
    pub dir: PathBuf,
    pub depth: u8,
    pub suffixes: &'static [&'static str],
    pub parent: Option<&'static str>,
}

impl Root {
    fn accepts(&self, dir: &Path, name: &str) -> bool {
        let in_parent = self
            .parent
            .is_none_or(|p| dir.file_name().is_some_and(|d| d == p));
        in_parent && (self.suffixes.is_empty() || self.suffixes.iter().any(|s| name.ends_with(s)))
    }
}

/// Set of recently modified files under some roots. Directory walks are throttled;
/// between walks only the known files are re-stat'ed.
pub struct FileSet {
    walked: Option<Instant>,
    walk_every: Duration,
    lookback: Duration,
    pub files: HashMap<PathBuf, Meta>,
}

impl FileSet {
    pub fn new(walk_every: Duration, lookback: Duration) -> Self {
        FileSet {
            walked: None,
            walk_every,
            lookback,
            files: HashMap::new(),
        }
    }

    pub fn refresh(&mut self, roots: &[Root]) {
        let cutoff = now_ms() - self.lookback.as_millis() as i64;
        if self.walked.is_none_or(|t| t.elapsed() >= self.walk_every) {
            self.walked = Some(Instant::now());
            self.files.clear();
            for root in roots {
                walk(&root.dir, root, root.depth, cutoff, &mut self.files);
            }
        } else {
            self.files.retain(|path, m| match meta(path) {
                Some(fresh) => {
                    *m = fresh;
                    true
                }
                None => false,
            });
        }
    }
}

fn walk(dir: &Path, root: &Root, depth: u8, cutoff: i64, out: &mut HashMap<PathBuf, Meta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth > 0 {
                walk(&path, root, depth - 1, cutoff, out);
            }
        } else if kind.is_file() && root.accepts(dir, &entry.file_name().to_string_lossy()) {
            if let Some(m) = meta(&path) {
                if m.mtime >= cutoff {
                    out.insert(path, m);
                }
            }
        }
    }
}

/// Reads a growing line-oriented file incrementally.
#[derive(Default)]
pub struct Tail {
    offset: Option<u64>,
}

impl Tail {
    /// Calls `f` for every complete line appended since the previous call. The first
    /// call (and any call after truncation) only looks at the last `initial` bytes.
    pub fn read(&mut self, path: &Path, len: u64, initial: u64, mut f: impl FnMut(&str)) {
        let mut skip_partial = false;
        let offset = match self.offset {
            Some(o) if o <= len => o,
            _ => {
                let o = len.saturating_sub(initial);
                skip_partial = o > 0;
                o
            }
        };
        if offset == len {
            self.offset = Some(offset);
            return;
        }
        let Ok(mut file) = File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((len - offset).min(8 << 20) as usize);
        if file.take(len - offset).read_to_end(&mut buf).is_err() {
            return;
        }
        let mut start = 0;
        if skip_partial {
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => start = i + 1,
                None => {
                    self.offset = Some(len);
                    return;
                }
            }
        }
        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            self.offset = Some(offset);
            return;
        };
        if last_nl >= start {
            for line in buf[start..last_nl].split(|&b| b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                if let Ok(s) = std::str::from_utf8(line) {
                    f(s);
                }
            }
        }
        self.offset = Some(offset + last_nl as u64 + 1);
    }
}

/// Reads up to `max` bytes from the end of a file.
pub fn read_tail(path: &Path, len: u64, max: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Reads the first line of a file (up to `max` bytes).
pub fn read_first_line(path: &Path, max: u64) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// Last path component, used as a human readable project name.
pub fn base_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    /// Unique scratch file for a test (tests run in parallel).
    pub fn tmp(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join("agentnight-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{}-{n}", std::process::id()))
    }

    #[test]
    fn tail_reads_only_new_complete_lines() {
        let path = tmp("tail");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "one").unwrap();
        write!(f, "two").unwrap();
        f.flush().unwrap();

        let mut tail = Tail::default();
        let mut seen = Vec::new();
        let len = meta(&path).unwrap().len;
        tail.read(&path, len, 1 << 20, |l| seen.push(l.to_string()));
        assert_eq!(seen, ["one"]);

        writeln!(f, "!").unwrap();
        writeln!(f, "three").unwrap();
        f.flush().unwrap();
        let len = meta(&path).unwrap().len;
        tail.read(&path, len, 1 << 20, |l| seen.push(l.to_string()));
        assert_eq!(seen, ["one", "two!", "three"]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_skips_partial_first_line_of_window() {
        let path = tmp("window");
        std::fs::write(&path, "aaaaaaaaaa\nbb\ncc\n").unwrap();
        let mut tail = Tail::default();
        let mut seen = Vec::new();
        let len = meta(&path).unwrap().len;
        tail.read(&path, len, 8, |l| seen.push(l.to_string()));
        assert_eq!(seen, ["bb", "cc"]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_set_respects_depth_suffix_and_parent() {
        let root = tmp("fileset");
        std::fs::create_dir_all(root.join("p1/chats")).unwrap();
        std::fs::create_dir_all(root.join("p1/deep/er/chats")).unwrap();
        for f in [
            "p1/chats/a.json",
            "p1/logs.json",
            "p1/chats/b.txt",
            "p1/deep/er/chats/c.json",
        ] {
            std::fs::write(root.join(f), "{}").unwrap();
        }
        let mut set = FileSet::new(Duration::from_secs(60), Duration::from_secs(60));
        set.refresh(&[Root {
            dir: root.clone(),
            depth: 2,
            suffixes: &[".json"],
            parent: Some("chats"),
        }]);
        let found: Vec<_> = set.files.keys().cloned().collect();
        assert_eq!(found, [root.join("p1").join("chats").join("a.json")]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn base_name_handles_both_separators() {
        assert_eq!(base_name("C:\\Users\\me\\proj"), "proj");
        assert_eq!(base_name("/home/me/proj/"), "proj");
    }
}
