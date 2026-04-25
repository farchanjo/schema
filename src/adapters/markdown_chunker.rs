//! Per-kind chunking strategies (`Markdown` / `ADR` / `Glossary` / `CUE` / `OpenAPI`).
//!
//! FASE 1.0 uses a conservative MVP strategy:
//!
//! - markdown family (`markdown`, `adr-madr`, `glossary`) — split on top-level
//!   headings (`#`, `##`); fall back to paragraph splits if a section exceeds
//!   `chunk_size_max`.
//! - `cue` and `openapi` — split on top-level keys / paths, by line.
//!
//! Code chunking via `tree-sitter` is FASE 2; this module returns
//! [`crate::ports::ChunkerError::UnsupportedKind`] if asked to chunk a kind it
//! does not yet handle.

use std::fs;
use std::mem;
use std::path::Path;

use crate::domain::{Chunk, CorpusKind};
use crate::ports::{Chunker, ChunkerError};

/// Markdown / line-block chunker. Stateless aside from `chunk_size_max`.
#[derive(Debug)]
pub struct MarkdownChunker {
    chunk_size_max: usize,
}

struct ChunkArgs<'a> {
    source_path: &'a str,
    line_start: usize,
    line_end: usize,
    title: Option<String>,
    content: String,
    kind: CorpusKind,
}

/// Context passed to [`MarkdownChunker::push_tail`]: captures the trailing
/// buffer state (`buf`, `buf_start`, `title`) plus the kind and original
/// `content` for line-count tail computation.
struct TailCtx<'a> {
    source_path: &'a str,
    kind: CorpusKind,
    buf_start: usize,
    title: Option<String>,
    buf: String,
    content: &'a str,
}

/// In-progress chunking buffers shared by `chunk_markdown` and
/// `chunk_line_blocks`.
struct ChunkLoopState {
    chunks: Vec<Chunk>,
    buf: String,
    buf_start: usize,
    title: Option<String>,
}

impl ChunkLoopState {
    const fn new() -> Self {
        Self {
            chunks: Vec::new(),
            buf: String::new(),
            buf_start: 1_usize,
            title: None,
        }
    }

    fn tail_ctx<'a>(
        &mut self,
        source_path: &'a str,
        kind: CorpusKind,
        content: &'a str,
    ) -> TailCtx<'a> {
        TailCtx {
            source_path,
            kind,
            buf_start: self.buf_start,
            title: self.title.take(),
            buf: mem::take(&mut self.buf),
            content,
        }
    }
}

impl MarkdownChunker {
    #[must_use]
    pub const fn new(chunk_size_max: usize) -> Self {
        Self { chunk_size_max }
    }

    fn chunk_adr(&self, source_path: &str, content: &str) -> Vec<Chunk> {
        let artifact_id = extract_adr_id(source_path).or_else(|| extract_adr_id(content));
        let mut chunks = self.chunk_markdown(source_path, content, CorpusKind::AdrMadr);
        for c in &mut chunks {
            c.artifact_id.clone_from(&artifact_id);
        }
        chunks
    }

    fn chunk_markdown(&self, source_path: &str, content: &str, kind: CorpusKind) -> Vec<Chunk> {
        let mut s = ChunkLoopState::new();
        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            if is_markdown_heading(line) {
                self.flush_md_section(&mut s, source_path, kind, line_num);
                s.title = Some(line.trim_start_matches('#').trim().to_string());
            }
            append_line(&mut s.buf, line);
        }
        let mut chunks = mem::take(&mut s.chunks);
        self.push_tail(&mut chunks, s.tail_ctx(source_path, kind, content));
        chunks
    }

    fn chunk_glossary(&self, source_path: &str, content: &str) -> Vec<Chunk> {
        self.chunk_markdown(source_path, content, CorpusKind::Glossary)
    }

    fn chunk_line_blocks(&self, source_path: &str, content: &str, kind: CorpusKind) -> Vec<Chunk> {
        let mut s = ChunkLoopState::new();
        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            if line.trim().is_empty() && !s.buf.is_empty() {
                self.flush_block(&mut s, source_path, kind, line_num);
                continue;
            }
            append_line(&mut s.buf, line);
        }
        let mut chunks = mem::take(&mut s.chunks);
        self.push_tail(&mut chunks, s.tail_ctx(source_path, kind, content));
        chunks
    }

    fn flush_md_section(
        &self,
        s: &mut ChunkLoopState,
        source_path: &str,
        kind: CorpusKind,
        line_num: usize,
    ) {
        if s.buf.is_empty() {
            return;
        }
        let args = chunk_args(
            source_path,
            kind,
            s.buf_start,
            line_num - 1,
            s.title.clone(),
            mem::take(&mut s.buf),
        );
        self.push_chunk_or_split(&mut s.chunks, args);
        s.buf_start = line_num;
    }

    fn flush_block(
        &self,
        s: &mut ChunkLoopState,
        source_path: &str,
        kind: CorpusKind,
        line_num: usize,
    ) {
        let args = chunk_args(
            source_path,
            kind,
            s.buf_start,
            line_num - 1,
            None,
            mem::take(&mut s.buf),
        );
        self.push_chunk_or_split(&mut s.chunks, args);
        s.buf_start = line_num + 1;
    }

    fn push_tail(&self, chunks: &mut Vec<Chunk>, ctx: TailCtx<'_>) {
        let TailCtx {
            source_path,
            kind,
            buf_start,
            title,
            buf,
            content,
        } = ctx;
        if buf.is_empty() {
            return;
        }
        let line_end = content.lines().count().max(1);
        self.push_chunk_or_split(
            chunks,
            chunk_args(source_path, kind, buf_start, line_end, title, buf),
        );
    }

    /// If the candidate fits within `chunk_size_max`, push it as one chunk;
    /// otherwise split on paragraph boundaries until each piece fits.
    fn push_chunk_or_split(&self, chunks: &mut Vec<Chunk>, args: ChunkArgs<'_>) {
        if args.content.len() <= self.chunk_size_max {
            chunks.push(Chunk {
                source_path: args.source_path.to_string(),
                line_start: args.line_start,
                line_end: args.line_end,
                artifact_id: None,
                title: args.title,
                content: args.content,
                kind: args.kind,
            });
            return;
        }
        self.split_oversize(chunks, args);
    }

    fn split_oversize(&self, chunks: &mut Vec<Chunk>, args: ChunkArgs<'_>) {
        let span = ChunkSpan {
            source_path: args.source_path,
            title: args.title.clone(),
            kind: args.kind,
        };
        let mut piece = String::new();
        let mut piece_start = args.line_start;
        let mut consumed = 0_usize;
        for para in args.content.split("\n\n") {
            self.absorb_paragraph(
                chunks,
                &span,
                &mut piece,
                &mut piece_start,
                &mut consumed,
                para,
            );
        }
        push_remaining(
            chunks,
            args.source_path,
            args.kind,
            piece_start,
            args.line_end,
            args.title,
            piece,
        );
    }

    /// Either append `para` to the in-flight `piece` or flush `piece` as a
    /// chunk (when it would otherwise exceed `chunk_size_max`) and start a new
    /// piece with `para`.
    fn absorb_paragraph(
        &self,
        chunks: &mut Vec<Chunk>,
        span: &ChunkSpan<'_>,
        piece: &mut String,
        piece_start: &mut usize,
        consumed_lines: &mut usize,
        para: &str,
    ) {
        let projected = if piece.is_empty() {
            para.len()
        } else {
            piece.len() + 2 + para.len()
        };
        if projected <= self.chunk_size_max || piece.is_empty() {
            if !piece.is_empty() {
                piece.push_str("\n\n");
                *consumed_lines += 1;
            }
            piece.push_str(para);
            *consumed_lines += para.lines().count();
            return;
        }
        let span_end = (*piece_start + *consumed_lines)
            .saturating_sub(1)
            .max(*piece_start);
        chunks.push(Chunk {
            source_path: span.source_path.to_string(),
            line_start: *piece_start,
            line_end: span_end,
            artifact_id: None,
            title: span.title.clone(),
            content: mem::take(piece),
            kind: span.kind,
        });
        *piece_start = span_end + 1;
        piece.push_str(para);
        *consumed_lines = para.lines().count();
    }
}

impl Chunker for MarkdownChunker {
    fn chunk(
        &self,
        relative_path: &Path,
        absolute_path: &Path,
        kind: CorpusKind,
    ) -> Result<Vec<Chunk>, ChunkerError> {
        let content = fs::read_to_string(absolute_path)?;
        let source_path = relative_path.to_string_lossy().to_string();

        let chunks = match kind {
            CorpusKind::AdrMadr => self.chunk_adr(&source_path, &content),
            CorpusKind::Markdown => self.chunk_markdown(&source_path, &content, kind),
            CorpusKind::Glossary => self.chunk_glossary(&source_path, &content),
            CorpusKind::Cue | CorpusKind::Openapi => {
                self.chunk_line_blocks(&source_path, &content, kind)
            }
        };

        Ok(chunks)
    }
}

/// Common header fields for a chunk produced by `split_oversize`.
struct ChunkSpan<'a> {
    source_path: &'a str,
    title: Option<String>,
    kind: CorpusKind,
}

fn push_remaining(
    chunks: &mut Vec<Chunk>,
    source_path: &str,
    kind: CorpusKind,
    piece_start: usize,
    line_end: usize,
    title: Option<String>,
    piece: String,
) {
    if piece.is_empty() {
        return;
    }
    chunks.push(Chunk {
        source_path: source_path.to_string(),
        line_start: piece_start,
        line_end,
        artifact_id: None,
        title,
        content: piece,
        kind,
    });
}

fn is_markdown_heading(line: &str) -> bool {
    line.starts_with("# ") || line.starts_with("## ") || line.starts_with("### ")
}

fn append_line(buf: &mut String, line: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(line);
}

const fn chunk_args(
    source_path: &str,
    kind: CorpusKind,
    line_start: usize,
    line_end: usize,
    title: Option<String>,
    content: String,
) -> ChunkArgs<'_> {
    ChunkArgs {
        source_path,
        line_start,
        line_end,
        title,
        content,
        kind,
    }
}

/// Try to extract an ADR id (e.g. `ADR-0055`) from a path or first heading.
fn extract_adr_id(text: &str) -> Option<String> {
    // Look for a literal "ADR-XXXX" pattern (4-5 digits).
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if &bytes[i..i + 4] == b"ADR-" {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 4 {
                return Some(String::from_utf8_lossy(&bytes[i..j]).into_owned());
            }
        }
        // Also handle filenames like "0055-something.md" → ADR-0055.
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j - i == 4
                && j < bytes.len()
                && (bytes[j] == b'-' || bytes[j] == b'_')
                && text.contains("decisions/")
            {
                return Some(format!("ADR-{}", &text[i..j]));
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]
    use super::*;

    #[test]
    fn extracts_adr_id_from_filename() {
        assert_eq!(
            extract_adr_id("docs/decisions/0055-something.md"),
            Some("ADR-0055".to_string())
        );
    }

    #[test]
    fn extracts_adr_id_from_heading() {
        assert_eq!(
            extract_adr_id("Some text mentioning ADR-0042 inline."),
            Some("ADR-0042".to_string())
        );
    }

    #[test]
    fn chunks_markdown_by_heading() {
        let chunker = MarkdownChunker::new(8_192);
        let md = "# Title\nIntro line\n\n## Section A\nA body\n\n## Section B\nB body";
        let chunks = chunker.chunk_markdown("doc.md", md, CorpusKind::Markdown);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].title.as_deref(), Some("Title"));
        assert_eq!(chunks[1].title.as_deref(), Some("Section A"));
        assert_eq!(chunks[2].title.as_deref(), Some("Section B"));
    }

    #[test]
    fn splits_oversize_chunk_on_paragraph_boundary() {
        let chunker = MarkdownChunker::new(50);
        let md = "# T\n\nfirst paragraph here\n\nsecond paragraph here\n\nthird paragraph here";
        let chunks = chunker.chunk_markdown("doc.md", md, CorpusKind::Markdown);
        // chunker should produce at least 2 chunks because total content > 50 bytes
        assert!(chunks.len() >= 2);
    }
}
