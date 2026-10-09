//! Read: file contents with line numbers (cat -n style), offset/limit
//! windows, and freshness ledger updates.

use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
/// Cap on total bytes returned by one read: line windows alone don't
/// bound the payload when lines are long.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Parse an optional positive-integer argument with a clear error.
fn positive_arg(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    match &args[key] {
        Value::Null => Ok(default),
        v => match v.as_u64() {
            Some(n) if n >= 1 => Ok(n as usize),
            _ => Err(format!("'{key}' must be a positive integer, got {v}")),
        },
    }
}

pub struct ReadTool {
    ws: Arc<Workspace>,
}

impl ReadTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn for_workspace(
        &self,
        session: &String,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace))))
    }
    fn name(&self) -> &str {
        "Read"
    }

    /// Read bounds itself (offset/limit, 256 KiB cap) and is how the model
    /// pages a spill file: spilling it again would loop.
    fn spills_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Read a file from the filesystem. Returns numbered lines (cat -n style). \
         Use offset (1-based line) and limit to window large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to the working directory)" },
                "offset": { "type": "integer", "description": "1-based line to start from (default 1)" },
                "limit": { "type": "integer", "description": "Max lines to return (default 2000)" },
            },
            "required": ["path"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        self.read_presented(args).await.map(|(output, _)| output)
    }

    async fn execute_presented(
        &self,
        _session: &String,
        _call: &String,
        args: Value,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        (
            Vec<rness_protocol::events::ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<Value>,
        ),
        String,
    > {
        let (output, presentation) = self.read_presented(args).await?;
        Ok((
            vec![rness_protocol::events::ToolResultContentPart::Text { text: output }],
            None,
            false,
            Some(presentation),
        ))
    }
}

impl ReadTool {
    async fn read_presented(&self, args: Value) -> Result<(String, Value), String> {
        let path = self.ws.resolve(required_str(&args, "path")?);
        let offset = positive_arg(&args, "offset", 1)?;
        let limit = positive_arg(&args, "limit", DEFAULT_LIMIT)?;

        // Stream on a blocking thread: memory is bounded by the requested
        // window, never by the file size (B2-5).
        let scan_path = path.clone();
        let scan = tokio::task::spawn_blocking(move || scan_window(&scan_path, offset, limit))
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))??;
        self.ws.mark_seen(&path);

        let total = scan.total;
        if total == 0 {
            return Ok((
                "(empty file)".to_string(),
                json!({"version":1,"kind":"read","path":path,"start_line":1,"total_lines":0,"text":"","truncated":false}),
            ));
        }
        if offset > total {
            return Err(format!(
                "offset {offset} is past the end of the file ({total} lines)"
            ));
        }

        let mut out = String::new();
        let mut snapshot = String::new();
        let mut snapshot_truncated = false;
        let mut shown_end = offset - 1;
        for (k, window_line) in scan.lines.iter().enumerate() {
            let i = offset - 1 + k;
            let line = window_line.text.as_str();
            if window_line.truncated {
                snapshot_truncated = true;
            }
            // Byte cap: stop before this line would push past it, so the
            // model gets whole numbered lines plus an accurate footer.
            if !out.is_empty() && out.len() + line.len() + 8 > MAX_OUTPUT_BYTES {
                break;
            }
            out.push_str(&format!("{:>6}\t{line}\n", i + 1));
            if snapshot.len() + line.len() + 1 <= 24 * 1024 && !snapshot_truncated {
                snapshot.push_str(line);
                snapshot.push('\n');
            } else {
                snapshot_truncated = true;
            }
            shown_end = i + 1;
        }
        if shown_end < total {
            out.push_str(&format!(
                "… {} more lines (file has {total} lines; continue with offset={})\n",
                total - shown_end,
                shown_end + 1,
            ));
        }
        let presentation = json!({
            "version":1,"kind":"read","path":path,
            "language":path.extension().and_then(|s| s.to_str()),
            "start_line":offset,"end_line":shown_end,"total_lines":total,
            "text":snapshot,"truncated":snapshot_truncated || shown_end < total,
        });
        Ok((out, presentation))
    }
}

/// One line of the requested window, cut to `MAX_LINE_LEN` bytes.
struct WindowLine {
    text: String,
    truncated: bool,
}

/// Result of one streaming pass: the total line count (`str::lines`
/// semantics) and only the lines inside the requested window.
struct Scan {
    total: usize,
    lines: Vec<WindowLine>,
}

/// Open `path` for reading only if it is a regular file. On Unix the open
/// is non-blocking so a FIFO is refused instead of waiting for a writer.
fn open_regular(path: &Path) -> Result<std::fs::File, String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts
        .open(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("read {}: is a directory", path.display()));
    }
    if !meta.is_file() {
        return Err(format!(
            "read {}: not a regular file (device, FIFO or socket); Read only reads regular files",
            path.display()
        ));
    }
    Ok(file)
}

/// Stream `path` once: validate UTF-8, count lines, and keep only lines
/// `offset..offset+limit` (each capped at `MAX_LINE_LEN`, at most
/// `MAX_OUTPUT_BYTES` in total). Memory is bounded by the window.
fn scan_window(path: &Path, offset: usize, limit: usize) -> Result<Scan, String> {
    let io_err = |e: std::io::Error| format!("read {}: {e}", path.display());
    let not_utf8 = || format!("{} is not valid UTF-8 (binary file?)", path.display());
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, open_regular(path)?);
    let first = offset - 1;
    let end = first.saturating_add(limit);
    let mut total = 0usize;
    // Current (not yet terminated) line: its full byte length and, while it
    // is in the window, its first `MAX_LINE_LEN + 1` bytes.
    let mut cur_len = 0usize;
    let mut cur = Vec::new();
    let mut lines = Vec::new();
    let mut stored = 0usize;
    let mut carry = Vec::new();
    loop {
        let buf = reader.fill_buf().map_err(io_err)?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        if !validate_utf8_chunk(&mut carry, buf) {
            return Err(not_utf8());
        }
        let mut rest = buf;
        while !rest.is_empty() {
            let in_window = total >= first && total < end && stored <= MAX_OUTPUT_BYTES;
            let (piece, terminated) = match rest.iter().position(|&b| b == b'\n') {
                Some(p) => (&rest[..p], true),
                None => (rest, false),
            };
            if in_window && cur.len() <= MAX_LINE_LEN {
                let room = MAX_LINE_LEN + 1 - cur.len();
                cur.extend_from_slice(&piece[..piece.len().min(room)]);
            }
            cur_len += piece.len();
            rest = &rest[piece.len() + usize::from(terminated)..];
            if terminated {
                if in_window {
                    let line = finish_line(&mut cur, cur_len, true);
                    stored += line.text.len();
                    lines.push(line);
                }
                total += 1;
                cur_len = 0;
            }
        }
        reader.consume(n);
    }
    if !carry.is_empty() {
        return Err(not_utf8());
    }
    if cur_len > 0 {
        if total >= first && total < end && stored <= MAX_OUTPUT_BYTES {
            lines.push(finish_line(&mut cur, cur_len, false));
        }
        total += 1;
    }
    Ok(Scan { total, lines })
}

/// Turn the stored prefix of a completed line into its window form, like
/// `str::lines`: strip the `\r` of a CRLF ending, then cut to
/// `MAX_LINE_LEN` at a char boundary. `cur` holds the first
/// `min(len, MAX_LINE_LEN + 1)` bytes; for a longer line the `\r` cannot
/// change the (already truncated) result.
fn finish_line(cur: &mut Vec<u8>, len: usize, newline: bool) -> WindowLine {
    let mut bytes = std::mem::take(cur);
    let mut len = len;
    if newline && len <= MAX_LINE_LEN + 1 && bytes.last() == Some(&b'\r') {
        bytes.pop();
        len -= 1;
    }
    let truncated = len > MAX_LINE_LEN;
    bytes.truncate(MAX_LINE_LEN);
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            // The cut split a multi-byte char: back off to its boundary.
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).expect("prefix is valid UTF-8")
        }
    };
    WindowLine { text, truncated }
}

/// Incremental UTF-8 validation across chunk boundaries: `carry` holds the
/// bytes of a character split by the previous chunk. Returns false on any
/// invalid sequence.
fn validate_utf8_chunk(carry: &mut Vec<u8>, mut chunk: &[u8]) -> bool {
    if !carry.is_empty() {
        let width = match carry[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return false,
        };
        let need = width - carry.len();
        if chunk.len() < need {
            carry.extend_from_slice(chunk);
            return true;
        }
        carry.extend_from_slice(&chunk[..need]);
        if std::str::from_utf8(carry).is_err() {
            return false;
        }
        carry.clear();
        chunk = &chunk[need..];
    }
    match std::str::from_utf8(chunk) {
        Ok(_) => true,
        Err(e) if e.error_len().is_none() => {
            carry.extend_from_slice(&chunk[e.valid_up_to()..]);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-streaming implementation: whole file + `str::lines`.
    fn reference(content: &str, offset: usize, limit: usize) -> (usize, Vec<(String, bool)>) {
        let lines = content
            .lines()
            .skip(offset - 1)
            .take(limit)
            .map(|line| {
                if line.len() > MAX_LINE_LEN {
                    let mut end = MAX_LINE_LEN;
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    (line[..end].to_owned(), true)
                } else {
                    (line.to_owned(), false)
                }
            })
            .collect();
        (content.lines().count(), lines)
    }

    fn scan(
        content: &[u8],
        offset: usize,
        limit: usize,
    ) -> Result<(usize, Vec<(String, bool)>), String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, content).unwrap();
        let s = scan_window(&path, offset, limit)?;
        Ok((
            s.total,
            s.lines.into_iter().map(|l| (l.text, l.truncated)).collect(),
        ))
    }

    #[test]
    fn streaming_window_matches_whole_file_lines() {
        let long = "x".repeat(MAX_LINE_LEN + 5);
        let long_euro = "€".repeat(MAX_LINE_LEN); // 3 bytes per char
        let exact = "y".repeat(MAX_LINE_LEN);
        let exact_cr = format!("{}\r", "z".repeat(MAX_LINE_LEN));
        let long_cr = format!("{}\r", "w".repeat(MAX_LINE_LEN + 1));
        let cases = [
            String::new(),
            "\n".into(),
            "a".into(),
            "a\nb".into(),
            "a\nb\n".into(),
            "a\r\nb\r\n\r\n".into(),
            "lone\rcr\n".into(),
            "trailing\r".into(),
            format!("{long}\nshort\n{long_euro}\n{exact}\n{exact_cr}\n{long_cr}\nend"),
            "€\n".repeat(70_000), // crosses 64 KiB chunks mid-char
        ];
        for content in &cases {
            for (offset, limit) in [(1, DEFAULT_LIMIT), (2, 1), (3, 3), (5, 2), (69_999, 5)] {
                let got = scan(content.as_bytes(), offset, limit).unwrap();
                assert_eq!(
                    got,
                    reference(content, offset, limit),
                    "{offset}/{limit} {:?}",
                    &content[..content.len().min(40)]
                );
            }
        }
    }

    #[test]
    fn invalid_utf8_anywhere_is_rejected() {
        let mut big = "ok\n".repeat(50_000).into_bytes();
        big.push(0xFF);
        for content in [&b"\xff\xfe"[..], b"a\n\xe2\x82", &big] {
            let err = scan(content, 1, 1).unwrap_err();
            assert!(err.contains("not valid UTF-8"), "{err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        for path in [
            fifo.as_path(),
            Path::new("/dev/zero"),
            Path::new("/dev/null"),
        ] {
            let err = scan_window(path, 1, 1).err().unwrap();
            assert!(err.contains("not a regular file"), "{err}");
        }
        let err = scan_window(dir.path(), 1, 1).err().unwrap();
        assert!(err.contains("is a directory"), "{err}");
    }
}
