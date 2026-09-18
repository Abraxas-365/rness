//! Workspace instructions (the dsh agent-instructions model, sized to
//! rness): AGENTS.md-chain discovery + a durable baseline user message.
//!
//! Mechanism only — WHICH filenames and HOW many bytes are the caller's
//! policy (composition root / Lua). Zero defaults here.
//!
//! The baseline is ordinary durable history (invariant #1): it replays,
//! forks, and compacts like any user message. Nothing protects it from
//! folding — instead, before each turn the service checks whether a
//! baseline with the current identity is VISIBLE in the projected
//! context and re-injects a fresh one (re-read from disk) when it is
//! not: first turn, post-compaction, or after the files changed.

use std::path::{Path, PathBuf};

/// Caller-owned policy. All fields required — no hidden defaults.
#[derive(Debug, Clone)]
pub struct InstructionsConfig {
    /// Session working directory (discovery anchor).
    pub cwd: PathBuf,
    /// Instruction file names, in precedence order per directory —
    /// first existing candidate wins (e.g. ["AGENTS.md", "CLAUDE.md"]).
    pub candidates: Vec<String>,
    /// Total byte budget for the rendered baseline. Broader files are
    /// dropped whole before the most-specific file is truncated.
    pub max_bytes: usize,
}

/// One discovered instruction file.
#[derive(Debug, Clone, PartialEq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub text: String,
}

/// The rendered baseline: what gets injected, plus the identity that
/// marks it current.
#[derive(Debug, Clone)]
pub struct Baseline {
    /// Fingerprint of discovery inputs + file contents. A visible
    /// baseline with this identity is current; anything else re-injects.
    pub identity: String,
    /// Rendered message text. Empty chain → None (inject nothing).
    pub text: String,
}

/// Find the project root: nearest ancestor of `cwd` (inclusive)
/// containing `.git`. Falls back to `cwd` itself.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

/// Discover the instruction chain: for each directory from the project
/// root down to `cwd`, the first existing candidate file. Broad → specific.
pub fn discover(config: &InstructionsConfig) -> Vec<InstructionFile> {
    let root = project_root(&config.cwd);
    // Directories root..=cwd (cwd outside root → just cwd).
    let mut dirs = vec![config.cwd.clone()];
    let mut d = config.cwd.as_path();
    while let Some(parent) = d.parent() {
        if !d.starts_with(&root) || d == root {
            break;
        }
        dirs.push(parent.to_path_buf());
        d = parent;
    }
    dirs.reverse();

    let mut found = Vec::new();
    for dir in dirs {
        for name in &config.candidates {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                found.push(InstructionFile { path, text });
                break; // first candidate wins per directory
            }
        }
    }
    found
}

/// Render the baseline message under the byte budget. dsh's budget
/// rule: if the chain exceeds `max_bytes`, drop the BROADEST files
/// whole first; only the most-specific file may be truncated.
pub fn render(config: &InstructionsConfig) -> Option<Baseline> {
    let files = discover(config);
    if files.is_empty() {
        return None;
    }

    // Budget: walk from most-specific backwards, keeping whole files
    // while they fit. The most-specific file may be truncated to fit.
    let mut kept: Vec<&InstructionFile> = Vec::new();
    let mut used = 0usize;
    for (i, f) in files.iter().enumerate().rev() {
        let is_most_specific = i == files.len() - 1;
        if used + f.text.len() <= config.max_bytes {
            used += f.text.len();
            kept.push(f);
        } else if is_most_specific {
            kept.push(f); // truncated at render below
            used = config.max_bytes;
        }
        // else: broader file dropped whole
    }
    kept.reverse();

    let mut body = String::new();
    let mut remaining = config.max_bytes;
    for f in &kept {
        let take = f.text.len().min(remaining);
        // Truncation only ever applies to the most-specific tail file.
        let mut chunk = f.text[..floor_char_boundary(&f.text, take)].to_string();
        if take < f.text.len() {
            chunk.push_str("\n[... truncated: instruction budget exceeded ...]");
        }
        body.push_str(&format!(
            "Instructions from: {}\n\n{}\n\n",
            f.path.display(),
            chunk
        ));
        remaining = remaining.saturating_sub(take);
    }

    let text = format!(
        "<system-reminder>\n\
         Workspace instructions apply to this session. They do not \
         override direct user instructions.\n\n\
         {}</system-reminder>",
        body
    );

    // Identity: discovery inputs + content. Any change re-injects.
    let mut hasher = Sha1::new();
    hasher.update(config.cwd.to_string_lossy().as_bytes());
    for c in &config.candidates {
        hasher.update(c.as_bytes());
    }
    hasher.update(config.max_bytes.to_le_bytes());
    for f in &files {
        hasher.update(f.path.to_string_lossy().as_bytes());
        hasher.update(f.text.as_bytes());
    }
    Some(Baseline {
        identity: hasher.hex(),
        text,
    })
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// -- minimal SHA-1 (identity fingerprint only, not security) ---------------

struct Sha1 {
    state: [u32; 5],
    buf: Vec<u8>,
    len: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: Vec::new(),
            len: 0,
        }
    }

    fn update(&mut self, data: impl AsRef<[u8]>) {
        let data = data.as_ref();
        self.len += data.len() as u64;
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 64 {
            let block: [u8; 64] = self.buf[..64].try_into().unwrap();
            self.compress(&block);
            self.buf.drain(..64);
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }

    fn hex(mut self) -> String {
        let len_bits = self.len * 8;
        self.buf.push(0x80);
        while self.buf.len() % 64 != 56 {
            self.buf.push(0);
        }
        self.buf.extend_from_slice(&len_bits.to_be_bytes());
        let blocks: Vec<[u8; 64]> = self.buf.chunks(64).map(|c| c.try_into().unwrap()).collect();
        for b in &blocks {
            self.compress(b);
        }
        self.state.iter().map(|w| format!("{w:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cwd: &Path, max: usize) -> InstructionsConfig {
        InstructionsConfig {
            cwd: cwd.to_path_buf(),
            candidates: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            max_bytes: max,
        }
    }

    #[test]
    fn sha1_known_vector() {
        let mut h = Sha1::new();
        h.update(b"abc");
        assert_eq!(h.hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn discovers_the_chain_broad_to_specific() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(root.join("sub/CLAUDE.md"), "sub rules").unwrap();

        let files = discover(&cfg(&root.join("sub/deeper"), 4096));
        let names: Vec<String> = files.iter().map(|f| f.text.clone()).collect();
        assert_eq!(names, vec!["root rules", "sub rules"]);
    }

    #[test]
    fn first_candidate_wins_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "agents").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "claude").unwrap();
        let files = discover(&cfg(root, 4096));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].text, "agents");
    }

    #[test]
    fn empty_chain_renders_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert!(render(&cfg(dir.path(), 4096)).is_none());
    }

    #[test]
    fn identity_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "v1").unwrap();
        let a = render(&cfg(root, 4096)).unwrap();
        std::fs::write(root.join("AGENTS.md"), "v2").unwrap();
        let b = render(&cfg(root, 4096)).unwrap();
        assert_ne!(a.identity, b.identity);
        assert!(b.text.contains("v2"));
    }

    #[test]
    fn budget_drops_broad_files_before_truncating_specific() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "B".repeat(100)).unwrap();
        std::fs::write(root.join("sub/AGENTS.md"), "S".repeat(80)).unwrap();

        // Budget fits only the specific file: broad one dropped whole.
        let b = render(&cfg(&root.join("sub"), 90)).unwrap();
        assert!(
            !b.text.contains(&"B".repeat(100)),
            "broad file must be dropped whole"
        );
        assert!(b.text.contains(&"S".repeat(80)));

        // Budget below the specific file alone: it truncates, marked.
        let b = render(&cfg(&root.join("sub"), 40)).unwrap();
        assert!(b.text.contains(&"S".repeat(40)));
        assert!(b.text.contains("truncated"));
    }
}
