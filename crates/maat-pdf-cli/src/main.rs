use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const PAGE_WIDTH: f32 = 612.0;
const PAGE_HEIGHT: f32 = 792.0;
const LEFT_MARGIN: f32 = 54.0;
const TOP_MARGIN: f32 = 64.0;
const TITLE_FONT_SIZE: f32 = 18.0;
const BODY_FONT_SIZE: f32 = 11.0;
const BODY_LINE_HEIGHT: f32 = 15.0;
const MAX_BODY_LINES_PER_PAGE: usize = 44;
const MAX_CHARS_PER_LINE: usize = 92;

#[derive(Debug, Default, Deserialize)]
struct SkillInput {
    #[serde(default)]
    request: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    output_path: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let raw = env::var("MAAT_SKILL_INPUT").unwrap_or_else(|_| "{}".into());
    let input: SkillInput = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid MAAT_SKILL_INPUT: {error}"))?;

    let workspace_dir = env::var("MAAT_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let workspace_dir = workspace_dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve workspace dir: {error}"))?;

    let title = input
        .title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Generated PDF".into());
    let content = input
        .content
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| input.request.clone());
    let output_path = input
        .output_path
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "output/pdf/generated.pdf".into());

    let destination = resolve_output_path(&workspace_dir, &output_path)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }

    let pdf_bytes = build_pdf(&title, &content);
    fs::write(&destination, pdf_bytes)
        .map_err(|error| format!("failed to write {}: {error}", destination.display()))?;

    let relative_path = destination
        .strip_prefix(&workspace_dir)
        .unwrap_or(&destination)
        .display()
        .to_string();
    println!(
        "{}",
        serde_json::json!({
            "status": "created",
            "path": relative_path,
            "title": title,
        })
    );
    Ok(())
}

fn resolve_output_path(workspace_dir: &Path, output_path: &str) -> Result<PathBuf, String> {
    let stripped = output_path.trim_start_matches('/');
    let candidate = workspace_dir.join(stripped);
    let parent = candidate
        .parent()
        .ok_or_else(|| "output path has no parent".to_string())?;
    let canonical_parent = parent
        .canonicalize()
        .or_else(|_| {
            fs::create_dir_all(parent)?;
            parent.canonicalize()
        })
        .map_err(|error| format!("failed to prepare output directory: {error}"))?;

    if !canonical_parent.starts_with(workspace_dir) {
        return Err(format!(
            "output path '{}' escapes the workspace",
            output_path
        ));
    }

    Ok(candidate)
}

fn build_pdf(title: &str, content: &str) -> Vec<u8> {
    let title_line = wrap_text(title, 60);
    let body_lines = wrap_text(content, MAX_CHARS_PER_LINE);

    let mut pages = Vec::new();
    let mut current = Vec::new();

    for (index, line) in title_line.into_iter().enumerate() {
        if index == 0 {
            current.push(PageLine {
                text: line,
                font_size: TITLE_FONT_SIZE,
                y: PAGE_HEIGHT - TOP_MARGIN,
            });
            current.push(PageLine {
                text: String::new(),
                font_size: BODY_FONT_SIZE,
                y: PAGE_HEIGHT - TOP_MARGIN - 24.0,
            });
        } else {
            let y = PAGE_HEIGHT - TOP_MARGIN - 24.0 - ((index as f32 - 1.0) * BODY_LINE_HEIGHT);
            current.push(PageLine {
                text: line,
                font_size: TITLE_FONT_SIZE,
                y,
            });
        }
    }

    let mut line_index = 0usize;
    let mut page_number = 1usize;
    while line_index < body_lines.len().max(1) {
        if current.is_empty() {
            current.push(PageLine {
                text: title.to_string(),
                font_size: TITLE_FONT_SIZE,
                y: PAGE_HEIGHT - TOP_MARGIN,
            });
        }

        let body_start_y = PAGE_HEIGHT - TOP_MARGIN - 40.0;
        let end = (line_index + MAX_BODY_LINES_PER_PAGE).min(body_lines.len());
        let chunk = if body_lines.is_empty() {
            vec![String::new()]
        } else {
            body_lines[line_index..end].to_vec()
        };
        for (offset, line) in chunk.into_iter().enumerate() {
            current.push(PageLine {
                text: line,
                font_size: BODY_FONT_SIZE,
                y: body_start_y - (offset as f32 * BODY_LINE_HEIGHT),
            });
        }
        current.push(PageLine {
            text: format!("Page {page_number}"),
            font_size: 10.0,
            y: 32.0,
        });
        pages.push(current);
        current = Vec::new();
        line_index = end;
        page_number += 1;
        if body_lines.is_empty() {
            break;
        }
    }

    render_pdf(pages)
}

#[derive(Clone)]
struct PageLine {
    text: String,
    font_size: f32,
    y: f32,
}

fn render_pdf(pages: Vec<Vec<PageLine>>) -> Vec<u8> {
    let font_object_id = 3 + (pages.len() * 2);
    let mut objects = Vec::new();

    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());

    let page_ids: Vec<usize> = (0..pages.len()).map(|index| 3 + (index * 2)).collect();
    let kids = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids,
        pages.len()
    ));

    for (index, page) in pages.iter().enumerate() {
        let page_id = 3 + (index * 2);
        let content_id = page_id + 1;
        let stream = page_stream(page);
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}] /Resources << /Font << /F1 {font_object_id} 0 R >> >> /Contents {content_id} 0 R >>"
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            stream.as_bytes().len(),
            stream
        ));
    }

    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());

    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = vec![0usize];

    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.as_bytes().len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }

    let xref_offset = pdf.as_bytes().len();
    pdf.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
    pdf.push_str("0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        objects.len() + 1,
        xref_offset
    ));

    pdf.into_bytes()
}

fn page_stream(lines: &[PageLine]) -> String {
    let mut stream = String::from("BT\n/F1 11 Tf\n");
    for line in lines {
        stream.push_str(&format!(
            "/F1 {} Tf\n1 0 0 1 {} {} Tm\n({}) Tj\n",
            line.font_size,
            LEFT_MARGIN,
            line.y,
            escape_pdf_text(&line.text)
        ));
    }
    stream.push_str("ET");
    stream
}

fn escape_pdf_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii() { ch } else { '?' })
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();

    for paragraph in text.lines() {
        if paragraph.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= max_chars {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }

        if !current.is_empty() {
            lines.push(current);
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_contains_basic_markers() {
        let bytes = build_pdf("Title", "Hello world");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("%PDF-1.4"));
        assert!(text.contains("/Type /Catalog"));
        assert!(text.contains("Hello world"));
    }
}
