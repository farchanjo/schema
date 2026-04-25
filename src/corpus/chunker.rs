//! Per-kind chunking strategies.
//!
//! FASE 1.0 uses a conservative MVP strategy:
//!
//! - markdown family (`markdown`, `adr-madr`, `glossary`) — split on top-level
//!   headings (`#`, `##`); fall back to paragraph splits if a section exceeds
//!   `chunk_size_max`.
//! - `cue` and `openapi` — split on top-level keys / paths, by line.
//!
//! Code chunking via `tree-sitter` is FASE 2; this module returns
//! [`ChunkerError::UnsupportedKind`] if asked to chunk a kind it does not yet
//! handle.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::CorpusKind;

#[derive(Debug, Error)]
pub enum ChunkerError {
    #[error("unsupported corpus kind: {0:?}")]
    UnsupportedKind(CorpusKind),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A chunk extracted from a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Path relative to the project root.
    pub source_path: String,
    /// 1-indexed line range (inclusive).
    pub line_start: usize,
    pub line_end: usize,
    /// Optional artifact id (e.g. `"ADR-0055"`) when extractable.
    pub artifact_id: Option<String>,
    /// Optional human-readable title (e.g. the section heading).
    pub title: Option<String>,
    /// The raw chunk text.
    pub content: String,
    /// The kind of the originating corpus entry.
    pub kind: CorpusKind,
}

#[derive(Debug)]
pub struct Chunker {
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

impl Chunker {
    pub fn new(chunk_size_max: usize) -> Self {
        Self { chunk_size_max }
    }

    pub fn chunk(
        &self,
        relative_path: &Path,
        absolute_path: &Path,
        kind: CorpusKind,
    ) -> Result<Vec<Chunk>, ChunkerError> {
        let content = std::fs::read_to_string(absolute_path)?;
        let source_path = relative_path.to_string_lossy().to_string();

        let chunks = match kind {
            CorpusKind::AdrMadr => self.chunk_adr(&source_path, &content),
            CorpusKind::Markdown => self.chunk_markdown(&source_path, &content, kind),
            CorpusKind::Glossary => self.chunk_glossary(&source_path, &content),
            CorpusKind::Cue => self.chunk_line_blocks(&source_path, &content, kind),
            CorpusKind::Openapi => self.chunk_line_blocks(&source_path, &content, kind),
        };

        Ok(chunks)
    }

    fn chunk_adr(&self, source_path: &str, content: &str) -> Vec<Chunk> {
        let artifact_id = extract_adr_id(source_path).or_else(|| extract_adr_id(content));
        let mut chunks = self.chunk_markdown(source_path, content, CorpusKind::AdrMadr);
        for c in &mut chunks {
            c.artifact_id = artifact_id.clone();
        }
        chunks
    }

    fn chunk_markdown(&self, source_path: &str, content: &str, kind: CorpusKind) -> Vec<Chunk> {
        // Split on top-level (`# `) and second-level (`## `) headings.
        let mut chunks = Vec::new();
        let mut buf = String::new();
        let mut buf_start = 1usize;
        let mut current_title: Option<String> = None;

        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            let is_heading =
                line.starts_with("# ") || line.starts_with("## ") || line.starts_with("### ");

            if is_heading && !buf.is_empty() {
                self.push_chunk_or_split(
                    &mut chunks,
                    ChunkArgs {
                        source_path,
                        line_start: buf_start,
                        line_end: line_num - 1,
                        title: current_title.clone(),
                        content: buf.clone(),
                        kind,
                    },
                );
                buf.clear();
                buf_start = line_num;
            }

            if is_heading {
                let stripped = line.trim_start_matches('#').trim();
                current_title = Some(stripped.to_string());
            }

            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(line);
        }

        if !buf.is_empty() {
            let line_end = content.lines().count().max(1);
            self.push_chunk_or_split(
                &mut chunks,
                ChunkArgs {
                    source_path,
                    line_start: buf_start,
                    line_end,
                    title: current_title,
                    content: buf,
                    kind,
                },
            );
        }

        chunks
    }

    fn chunk_glossary(&self, source_path: &str, content: &str) -> Vec<Chunk> {
        // Glossary is markdown with one term per `## ` heading.
        self.chunk_markdown(source_path, content, CorpusKind::Glossary)
    }

    fn chunk_line_blocks(&self, source_path: &str, content: &str, kind: CorpusKind) -> Vec<Chunk> {
        // For CUE and OpenAPI: split on blank lines as block boundaries; cap
        // each chunk at `chunk_size_max`.
        let mut chunks = Vec::new();
        let mut buf = String::new();
        let mut buf_start = 1usize;

        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            if line.trim().is_empty() && !buf.is_empty() {
                self.push_chunk_or_split(
                    &mut chunks,
                    ChunkArgs {
                        source_path,
                        line_start: buf_start,
                        line_end: line_num - 1,
                        title: None,
                        content: buf.clone(),
                        kind,
                    },
                );
                buf.clear();
                buf_start = line_num + 1;
                continue;
            }
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(line);
        }

        if !buf.is_empty() {
            let line_end = content.lines().count().max(1);
            self.push_chunk_or_split(
                &mut chunks,
                ChunkArgs {
                    source_path,
                    line_start: buf_start,
                    line_end,
                    title: None,
                    content: buf,
                    kind,
                },
            );
        }

        chunks
    }

    /// If the candidate fits within `chunk_size_max`, push it as one chunk;
    /// otherwise split on paragraph boundaries until each piece fits.
    fn push_chunk_or_split(&self, chunks: &mut Vec<Chunk>, args: ChunkArgs<'_>) {
        let ChunkArgs {
            source_path,
            line_start,
            line_end,
            title,
            content,
            kind,
        } = args;
        if content.len() <= self.chunk_size_max {
            chunks.push(Chunk {
                source_path: source_path.to_string(),
                line_start,
                line_end,
                artifact_id: None,
                title,
                content,
                kind,
            });
            return;
        }

        // Greedy split on `\n\n` to keep paragraph boundaries.
        let paragraphs: Vec<&str> = content.split("\n\n").collect();
        let mut piece = String::new();
        let mut piece_start = line_start;
        let mut consumed_lines = 0;

        for para in paragraphs {
            let projected = if piece.is_empty() {
                para.len()
            } else {
                piece.len() + 2 + para.len()
            };
            if projected <= self.chunk_size_max || piece.is_empty() {
                if !piece.is_empty() {
                    piece.push_str("\n\n");
                    consumed_lines += 1; // for the blank line between paragraphs
                }
                piece.push_str(para);
                consumed_lines += para.lines().count();
            } else {
                let span_end = (piece_start + consumed_lines)
                    .saturating_sub(1)
                    .max(piece_start);
                chunks.push(Chunk {
                    source_path: source_path.to_string(),
                    line_start: piece_start,
                    line_end: span_end,
                    artifact_id: None,
                    title: title.clone(),
                    content: std::mem::take(&mut piece),
                    kind,
                });
                piece_start = span_end + 1;
                piece.push_str(para);
                consumed_lines = para.lines().count();
            }
        }

        if !piece.is_empty() {
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
        let chunker = Chunker::new(8_192);
        let md = "# Title\nIntro line\n\n## Section A\nA body\n\n## Section B\nB body";
        let chunks = chunker.chunk_markdown("doc.md", md, CorpusKind::Markdown);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].title.as_deref(), Some("Title"));
        assert_eq!(chunks[1].title.as_deref(), Some("Section A"));
        assert_eq!(chunks[2].title.as_deref(), Some("Section B"));
    }

    #[test]
    fn splits_oversize_chunk_on_paragraph_boundary() {
        let chunker = Chunker::new(50);
        let md = "# T\n\nfirst paragraph here\n\nsecond paragraph here\n\nthird paragraph here";
        let chunks = chunker.chunk_markdown("doc.md", md, CorpusKind::Markdown);
        // chunker should produce at least 2 chunks because total content > 50 bytes
        assert!(chunks.len() >= 2);
    }
}
