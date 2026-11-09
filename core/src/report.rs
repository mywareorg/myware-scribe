//! Self-contained report documents.
//!
//! A report is a short, styled document: a title, one provenance line, then
//! headings, prose and bullets. It is written here rather than through the
//! transcript exporter because it is not a sequence of cues, and because the
//! three output formats share one block list.
//!
//! The PDF emitter is intentionally hand-rolled. Core carries no PDF
//! dependency, and a report only needs Helvetica, a handful of text operators
//! and a correct xref, so a few hundred lines keep the build dependency-free.
//! Text outside WinAnsi is transliterated or replaced, which is the same
//! licence-clean trade the DOCX writer makes with fonts.

use std::fs;
use std::io;
use std::path::Path;

use crate::docx;

/// One styled piece of a report document.
#[derive(Debug, Clone, PartialEq)]
pub enum ReportBlock {
    /// The document's title: largest type, bold.
    Title(String),
    /// A small provenance line under the title (date, length).
    Meta(String),
    /// A section heading.
    Heading(String),
    /// A wrapped body paragraph.
    Paragraph(String),
    /// One bullet list item.
    Bullet(String),
}

/// The report's file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Txt,
    Docx,
    Pdf,
}

impl ReportFormat {
    pub const ALL: [ReportFormat; 3] = [ReportFormat::Txt, ReportFormat::Docx, ReportFormat::Pdf];

    pub fn label(self) -> &'static str {
        match self {
            ReportFormat::Txt => "Plain text",
            ReportFormat::Docx => "Word document",
            ReportFormat::Pdf => "PDF",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ReportFormat::Txt => "txt",
            ReportFormat::Docx => "docx",
            ReportFormat::Pdf => "pdf",
        }
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        match extension.as_str() {
            "txt" => Some(ReportFormat::Txt),
            "docx" => Some(ReportFormat::Docx),
            "pdf" => Some(ReportFormat::Pdf),
            _ => None,
        }
    }
}

/// Write `blocks` to `path` in `format`.
pub fn write(path: &Path, format: ReportFormat, blocks: &[ReportBlock]) -> io::Result<()> {
    match format {
        ReportFormat::Txt => fs::write(path, render_txt(blocks).as_bytes()),
        ReportFormat::Docx => docx::write_package(path, &render_docx(blocks)),
        ReportFormat::Pdf => fs::write(path, render_pdf(blocks).as_bytes()),
    }
}

// ---- Plain text ----------------------------------------------------------

fn render_txt(blocks: &[ReportBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        // The title and the meta line form one unspaced header; every block
        // after it is its own paragraph, so it gets a blank line before it.
        let header = matches!(block, ReportBlock::Title(_) | ReportBlock::Meta(_));
        if !header && !out.is_empty() {
            out.push('\n');
        }
        match block {
            ReportBlock::Title(text) | ReportBlock::Meta(text) => out.push_str(text),
            ReportBlock::Heading(text) => out.push_str(&text.to_uppercase()),
            ReportBlock::Paragraph(text) => out.push_str(text),
            ReportBlock::Bullet(text) => {
                out.push_str("- ");
                out.push_str(text);
            }
        }
        out.push('\n');
    }
    out
}

// ---- Word document -------------------------------------------------------

fn render_docx(blocks: &[ReportBlock]) -> String {
    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );

    for block in blocks {
        match block {
            ReportBlock::Title(text) => {
                paragraph(&mut body, text, 32, true, false, false);
            }
            ReportBlock::Meta(text) => {
                paragraph(&mut body, text, 18, false, true, false);
            }
            ReportBlock::Heading(text) => {
                paragraph(&mut body, text, 26, true, false, false);
            }
            ReportBlock::Paragraph(text) => {
                paragraph(&mut body, text, 22, false, false, false);
            }
            ReportBlock::Bullet(text) => {
                paragraph(&mut body, text, 22, false, false, true);
            }
        }
    }

    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1134" w:right="1134" w:bottom="1134" w:left="1134"/></w:sectPr>"#,
    );
    body.push_str("</w:body></w:document>");
    body
}

/// Append one run-only paragraph. Sizes are half-points, as Word counts them.
fn paragraph(body: &mut String, text: &str, size: u32, bold: bool, grey: bool, bullet: bool) {
    body.push_str("<w:p><w:pPr>");
    if docx::is_rtl(text) {
        body.push_str("<w:bidi/>");
    }
    body.push_str(r#"<w:spacing w:after="120"/></w:pPr><w:r><w:rPr>"#);
    if bold {
        body.push_str("<w:b/>");
    }
    if grey {
        body.push_str(r#"<w:color w:val="666666"/>"#);
    }
    body.push_str(&format!(
        r#"<w:sz w:val="{size}"/><w:szCs w:val="{size}"/></w:rPr><w:t xml:space="preserve">"#
    ));
    if bullet {
        body.push_str("\u{2022} ");
    }
    body.push_str(&docx::escape(text));
    body.push_str("</w:t></w:r></w:p>");
}

// ---- PDF -----------------------------------------------------------------

const PAGE_WIDTH: f32 = 612.0;
const PAGE_HEIGHT: f32 = 792.0;
const MARGIN: f32 = 54.0;
const TEXT_WIDTH: f32 = PAGE_WIDTH - 2.0 * MARGIN;
const BODY_SIZE: f32 = 11.0;
const TITLE_SIZE: f32 = 20.0;
const HEADING_SIZE: f32 = 13.0;
const META_SIZE: f32 = 9.0;
const LINE_HEIGHT: f32 = 1.45;
const PARAGRAPH_GAP: f32 = 6.0;
const BULLET_INDENT: f32 = 12.0;

/// Accumulates text operators and starts a new page when the next line would
/// cross the bottom margin.
struct PageBuilder {
    pages: Vec<String>,
    current: String,
    /// Top of the next line box, in PDF units from the bottom edge.
    y: f32,
    empty: bool,
}

impl PageBuilder {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            current: String::new(),
            y: PAGE_HEIGHT - MARGIN,
            empty: true,
        }
    }

    fn new_page(&mut self) {
        if !self.current.is_empty() {
            self.pages.push(std::mem::take(&mut self.current));
        }
        self.y = PAGE_HEIGHT - MARGIN;
        self.empty = true;
    }

    fn space_before(&mut self, amount: f32) {
        if !self.empty {
            self.y -= amount;
        }
    }

    fn space_after(&mut self, amount: f32) {
        if !self.empty {
            self.y -= amount;
        }
    }

    fn add_line(&mut self, text: &str, size: f32, bold: bool, indent: f32) {
        let height = size * LINE_HEIGHT;
        if !self.empty && self.y - height < MARGIN {
            self.new_page();
        }
        self.y -= size;
        let baseline = self.y;
        self.y -= height - size;
        let font = if bold { "F2" } else { "F1" };
        self.current.push_str(&format!(
            "BT /{font} {size:.2} Tf {:.2} {:.2} Td ({}) Tj ET\n",
            MARGIN + indent,
            baseline,
            pdf_escape(text)
        ));
        self.empty = false;
    }

    fn finish(mut self) -> Vec<String> {
        if !self.current.is_empty() {
            self.pages.push(self.current);
        } else if self.pages.is_empty() {
            self.pages.push(String::new());
        }
        self.pages
    }
}

fn render_pdf(blocks: &[ReportBlock]) -> String {
    let mut page = PageBuilder::new();
    for block in blocks {
        match block {
            ReportBlock::Title(text) => {
                for line in wrap(text, TITLE_SIZE, TEXT_WIDTH, TEXT_WIDTH) {
                    page.add_line(&line, TITLE_SIZE, true, 0.0);
                }
            }
            ReportBlock::Meta(text) => {
                for line in wrap(text, META_SIZE, TEXT_WIDTH, TEXT_WIDTH) {
                    page.add_line(&line, META_SIZE, false, 0.0);
                }
            }
            ReportBlock::Heading(text) => {
                page.space_before(PARAGRAPH_GAP);
                for line in wrap(text, HEADING_SIZE, TEXT_WIDTH, TEXT_WIDTH) {
                    page.add_line(&line, HEADING_SIZE, true, 0.0);
                }
            }
            ReportBlock::Paragraph(text) => {
                for line in wrap(text, BODY_SIZE, TEXT_WIDTH, TEXT_WIDTH) {
                    page.add_line(&line, BODY_SIZE, false, 0.0);
                }
                page.space_after(PARAGRAPH_GAP);
            }
            ReportBlock::Bullet(text) => {
                let prefix = "\u{2022} ";
                let prefix_width = measure(prefix, BODY_SIZE);
                let first_width = TEXT_WIDTH - prefix_width;
                let rest_width = TEXT_WIDTH - BULLET_INDENT;
                for (index, line) in wrap(text, BODY_SIZE, first_width, rest_width)
                    .into_iter()
                    .enumerate()
                {
                    if index == 0 {
                        page.add_line(&format!("{prefix}{line}"), BODY_SIZE, false, 0.0);
                    } else {
                        page.add_line(&line, BODY_SIZE, false, BULLET_INDENT);
                    }
                }
            }
        }
    }

    assemble_pdf(page.finish())
}

/// Build the object table, xref and trailer around the finished page streams.
fn assemble_pdf(pages: Vec<String>) -> String {
    let page_count = pages.len();
    let helvetica = 3 + 2 * page_count;
    let helvetica_bold = helvetica + 1;

    let mut objects: Vec<String> = Vec::with_capacity(2 + 2 * page_count + 2);
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    let kids = (0..page_count)
        .map(|index| format!("{} 0 R", 3 + 2 * index))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!(
        "<< /Type /Pages /Kids [{kids}] /Count {page_count} >>"
    ));
    for (index, content) in pages.iter().enumerate() {
        let content_object = 4 + 2 * index;
        objects.push(format!(
            "<< /Type /Page\n/Parent 2 0 R\n/MediaBox [0 0 {PAGE_WIDTH:.0} {PAGE_HEIGHT:.0}]\n\
             /Resources << /Font << /F1 {helvetica} 0 R /F2 {helvetica_bold} 0 R >> >>\n\
             /Contents {content_object} 0 R >>"
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            content
        ));
    }
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    );
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .to_string(),
    );

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }

    let xref = out.len();
    out.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
    out.push_str("0000000000 65535 f \n");
    for offset in offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    out
}

/// Rough Helvetica advance width. Exact metrics would need a font table; these
/// fractions keep wrapping within a percent or two without one.
fn char_width(character: char, size: f32) -> f32 {
    let em = match character {
        'i' | 'l' | '.' | ',' | '\'' | '!' | '|' => 0.28,
        'm' | 'M' | 'w' | 'W' => 0.78,
        'A'..='Z' => 0.66,
        '0'..='9' => 0.556,
        _ => 0.5,
    };
    em * size
}

fn measure(text: &str, size: f32) -> f32 {
    text.chars().map(|c| char_width(c, size)).sum()
}

/// Greedy word wrap with a different width for the first line (bullets).
///
/// A word wider than the column is hard-split so no emitted line can exceed
/// the text area.
fn wrap(text: &str, size: f32, first_width: f32, rest_width: f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut width = 0.0f32;
    let space_width = char_width(' ', size);

    for word in text.split_whitespace() {
        let word_width = measure(word, size);
        let limit = if lines.is_empty() {
            first_width
        } else {
            rest_width
        };
        if !line.is_empty() {
            if width + space_width + word_width <= limit {
                line.push(' ');
                line.push_str(word);
                width += space_width + word_width;
                continue;
            }
            lines.push(std::mem::take(&mut line));
            width = 0.0;
        }

        let limit = if lines.is_empty() {
            first_width
        } else {
            rest_width
        };
        if word_width <= limit {
            line.push_str(word);
            width = word_width;
        } else {
            for character in word.chars() {
                let advance = char_width(character, size);
                let limit = if lines.is_empty() {
                    first_width
                } else {
                    rest_width
                };
                if !line.is_empty() && width + advance > limit {
                    lines.push(std::mem::take(&mut line));
                    width = 0.0;
                }
                line.push(character);
                width += advance;
            }
        }
    }

    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Escape one PDF literal string, mapping the few non-WinAnsi characters report
/// text is likely to carry and replacing the rest.
///
/// Output is pure ASCII: high bytes go out as octal escapes so the content
/// stream stays safe to copy through any transport.
fn pdf_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\n' | '\r' | '\t' => out.push(' '),
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2212}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            '\u{2022}' => out.push_str("\\225"),
            '\u{00A0}' => out.push(' '),
            character if character.is_ascii() => {
                if (' '..='~').contains(&character) {
                    out.push(character);
                } else {
                    out.push(' ');
                }
            }
            character => {
                let code = character as u32;
                if (0xA0..=0xFF).contains(&code) {
                    out.push_str(&format!("\\{code:03o}"));
                } else {
                    out.push('?');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hippo-report-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Vec<ReportBlock> {
        vec![
            ReportBlock::Title("Quarterly report".to_string()),
            ReportBlock::Meta("2026-11-09 - 12 minutes".to_string()),
            ReportBlock::Heading("Summary".to_string()),
            ReportBlock::Paragraph("The team shipped three items.".to_string()),
            ReportBlock::Bullet("first point".to_string()),
            ReportBlock::Bullet("second point".to_string()),
        ]
    }

    #[test]
    fn txt_has_a_header_uppercase_heading_and_bullets() {
        let dir = temp_dir("txt");
        let path = dir.join("report.txt");
        write(&path, ReportFormat::Txt, &sample()).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("Quarterly report\n"), "{text}");
        assert!(text.contains("SUMMARY\n"), "{text}");
        assert!(text.contains("- first point\n"), "{text}");
        assert!(text.ends_with('\n'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn docx_is_a_readable_package_with_the_content() {
        let dir = temp_dir("docx");
        let path = dir.join("report.docx");
        write(&path, ReportFormat::Docx, &sample()).unwrap();

        let file = fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut xml = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();

        assert!(xml.contains("Quarterly report"), "{xml}");
        assert!(xml.contains("Summary"), "{xml}");
        assert!(xml.contains("first point"), "{xml}");
        assert!(xml.contains("\u{2022} first point"), "{xml}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pdf_is_well_formed_and_paginates() {
        let dir = temp_dir("pdf");
        let path = dir.join("report.pdf");
        write(&path, ReportFormat::Pdf, &sample()).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("startxref"));
        assert!(text.contains("Quarterly report"));

        let long = std::iter::once(ReportBlock::Title("Long report".to_string()))
            .chain((0..200).map(|index| {
                ReportBlock::Paragraph(format!(
                    "Paragraph {index} has enough words to wrap across the page and fill it up."
                ))
            }))
            .collect::<Vec<_>>();
        let long_path = dir.join("long.pdf");
        write(&long_path, ReportFormat::Pdf, &long).unwrap();
        let long_text = String::from_utf8(fs::read(&long_path).unwrap()).unwrap();
        assert!(
            long_text.matches("/Type /Page\n").count() >= 2,
            "expected two pages"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extensions_round_trip_case_insensitively() {
        for format in ReportFormat::ALL {
            assert_eq!(
                ReportFormat::from_extension(format.extension()),
                Some(format)
            );
            assert_eq!(
                ReportFormat::from_extension(&format!(".{}", format.extension().to_uppercase())),
                Some(format)
            );
        }
        assert_eq!(ReportFormat::from_extension("doc"), None);
        assert_eq!(ReportFormat::from_extension(""), None);
    }
}
