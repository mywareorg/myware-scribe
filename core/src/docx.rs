//! A minimal DOCX writer for transcripts.
//!
//! Word documents are a zip of OOXML parts. Only the three parts Word and
//! LibreOffice require are written: content types, the package relationship,
//! and the body. Formatting is inline (bold speaker, grey timestamp), so no
//! styles part is needed.
//!
//! There is no font embedding and no PDF: Word substitutes fonts for scripts
//! it does not have, which is the trade this makes to stay small and licence-
//! clean.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;

use crate::export::Cue;

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

/// Write `cues` as a Word document.
pub fn write(path: &Path, cues: &[Cue], duration: Option<f32>) -> io::Result<()> {
    write_package(path, &document_xml(cues, duration))
}

/// Write a completed `word/document.xml` into the minimal OOXML package.
///
/// Shared with the report writer so both document formats carry exactly the
/// same three parts Word needs; splitting it here keeps the package details in
/// one place.
pub(crate) fn write_package(path: &Path, document: &str) -> io::Result<()> {
    let file = File::create(path)?;
    let mut archive = zip::ZipWriter::new(file);
    let options =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    add(&mut archive, "[Content_Types].xml", CONTENT_TYPES, options)?;
    add(&mut archive, "_rels/.rels", RELS, options)?;
    add(&mut archive, "word/document.xml", document, options)?;
    archive.finish().map_err(io::Error::other)?;
    Ok(())
}

fn add(
    archive: &mut zip::ZipWriter<File>,
    name: &str,
    contents: &str,
    options: SimpleFileOptions,
) -> io::Result<()> {
    archive
        .start_file(name, options)
        .map_err(io::Error::other)?;
    archive.write_all(contents.as_bytes())
}

fn document_xml(cues: &[Cue], duration: Option<f32>) -> String {
    let use_hours = duration.is_some_and(|seconds| seconds >= 3600.0)
        || cues
            .iter()
            .any(|cue| cue.start.is_some_and(|start| start >= 3600.0));

    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );

    for cue in cues {
        let rtl = is_rtl(&cue.text);
        body.push_str("<w:p><w:pPr>");
        if rtl {
            body.push_str("<w:bidi/>");
        }
        body.push_str(r#"<w:spacing w:after="120"/></w:pPr>"#);

        if !cue.speaker.is_empty() {
            body.push_str(r#"<w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">"#);
            body.push_str(&escape(&cue.speaker));
            body.push_str("</w:t></w:r>");
            body.push_str(r#"<w:r><w:t xml:space="preserve">  </w:t></w:r>"#);
        }
        if let Some(start) = cue.start {
            body.push_str(
                r#"<w:r><w:rPr><w:color w:val="808080"/></w:rPr><w:t xml:space="preserve">"#,
            );
            body.push_str(&escape(&format!("[{}] ", stamp(start, use_hours))));
            body.push_str("</w:t></w:r>");
        }
        body.push_str(r#"<w:r><w:t xml:space="preserve">"#);
        body.push_str(&escape(&cue.text));
        body.push_str("</w:t></w:r></w:p>");
    }

    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1134" w:right="1134" w:bottom="1134" w:left="1134"/></w:sectPr>"#,
    );
    body.push_str("</w:body></w:document>");
    body
}

fn stamp(seconds: f32, use_hours: bool) -> String {
    let total = seconds.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if use_hours || hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// True when the first strong character belongs to a right-to-left script.
pub(crate) fn is_rtl(text: &str) -> bool {
    for character in text.chars() {
        let code = character as u32;
        if (0x0590..=0x05FF).contains(&code) || (0x0600..=0x06FF).contains(&code) {
            return true;
        }
        if character.is_alphabetic() {
            return false;
        }
    }
    false
}

/// XML text escaping, with control characters removed so Word can open it.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\r' | '\t' => out.push(' '),
            character if character.is_control() => {}
            character => out.push(character),
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
            "myware-docx-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn document(path: &Path) -> String {
        let file = File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut xml = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        xml
    }

    #[test]
    fn writes_a_readable_package_with_escaped_text() {
        let dir = temp_dir("escape");
        let path = dir.join("out.docx");
        let cues = vec![Cue {
            start: Some(65.0),
            end: Some(70.0),
            speaker: "A&B".to_string(),
            text: "1 < 2 & \"three\"".to_string(),
            words: Vec::new(),
        }];
        write(&path, &cues, Some(120.0)).unwrap();

        let xml = document(&path);
        assert!(xml.contains("<w:b/>"));
        assert!(xml.contains("A&amp;B"));
        assert!(xml.contains("1 &lt; 2 &amp; &quot;three&quot;"));
        assert!(xml.contains("[01:05]"));
        assert!(!xml.contains("<w:bidi/>"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn right_to_left_text_gets_a_bidi_paragraph() {
        let dir = temp_dir("rtl");
        let path = dir.join("out.docx");
        let cues = vec![Cue {
            start: None,
            end: None,
            speaker: String::new(),
            text: "مرحبا بالعالم".to_string(),
            words: Vec::new(),
        }];
        write(&path, &cues, None).unwrap();
        assert!(document(&path).contains("<w:bidi/>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(escape("a\u{0}b\u{7}c"), "abc");
        assert_eq!(escape("line\nbreak"), "line break");
    }
}
