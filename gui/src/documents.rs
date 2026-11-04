use iced::widget::text_editor;

/// A single rendered transcript line.
#[derive(Debug, Clone)]
pub(crate) struct DocLine {
    pub(crate) raw: String,
    pub(crate) speaker: String,
    pub(crate) start: Option<f32>,
    /// Mean token log-probability from the ASR, when known. Less than zero;
    /// lower means less certain. `None` for transcripts without a sidecar.
    pub(crate) confidence: Option<f32>,
}

/// One editable row of the line editor: the transcript line it mirrors, plus
/// the speaker and text being edited for it.
pub(crate) struct LineEditRow {
    pub(crate) index: usize,
    pub(crate) speaker: String,
    pub(crate) content: text_editor::Content,
}

/// Inline line editor: the line being edited in place, plus the single row
/// that holds its text and speaker. `index` is the index of that row.
pub(crate) struct LineEdit {
    pub(crate) index: usize,
    pub(crate) rows: Vec<LineEditRow>,
}

impl LineEdit {
    /// Open the editor on `index` with no rows but that one, so a transcript
    /// row can become a field where it stands. `None` when `index` is out of
    /// range.
    pub(crate) fn inline(index: usize, lines: &[DocLine]) -> Option<Self> {
        Self::inline_at(index, lines, usize::MAX)
    }

    /// Open the editor on `index` with the caret `caret` bytes into the line's
    /// text. `usize::MAX` puts the caret at the end, which is where a fresh edit
    /// starts; a merge or a split passes the join point so the caret lands
    /// where the two halves met rather than at the far end of the line.
    pub(crate) fn inline_at(index: usize, lines: &[DocLine], caret: usize) -> Option<Self> {
        let line = lines.get(index)?;
        Some(Self {
            index,
            rows: vec![LineEditRow {
                index,
                speaker: line.speaker.clone(),
                content: content_at(&line_text(line), caret),
            }],
        })
    }

    /// Position of the current row within `rows`, which the messages and the
    /// merge/split/delete actions all address.
    pub(crate) fn current_position(&self) -> usize {
        self.rows
            .iter()
            .position(|row| row.index == self.index)
            .unwrap_or(0)
    }

    pub(crate) fn current_row(&self) -> Option<&LineEditRow> {
        let position = self.current_position();
        self.rows.get(position)
    }
}

/// An editor holding `text` with the caret `caret` bytes in; past the end puts
/// it at the end.
fn content_at(text: &str, caret: usize) -> text_editor::Content {
    let mut content = text_editor::Content::with_text(text);
    if caret >= text.len() {
        content.perform(text_editor::Action::Move(text_editor::Motion::End));
        return content;
    }
    content.perform(text_editor::Action::Move(text_editor::Motion::Home));
    let mut bytes = 0;
    let mut offset = 0;
    for character in text.chars() {
        if bytes >= caret {
            break;
        }
        bytes += character.len_utf8();
        offset += 1;
    }
    for _ in 0..offset {
        content.perform(text_editor::Action::Move(text_editor::Motion::Right));
    }
    content
}

/// The spoken text of a rendered line, without its timestamp or speaker label.
///
/// The label is recognised by its shape rather than by comparing it with
/// `line.speaker`: after a rename the stored line can still carry the old
/// label, and a merge or split must not fold that label into the spoken text.
pub(crate) fn line_text(line: &DocLine) -> String {
    let mut rest = line.raw.as_str();
    if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            rest = stripped[end + 1..].trim_start();
        }
    }
    strip_speaker_label(rest).to_string()
}

/// The longest leading token we accept as a speaker label.
const MAX_LABEL_CHARS: usize = 24;

/// Drop a leading `Label: ` prefix: a short token before the first colon.
///
/// Anything longer, empty, or carrying a `[` (a leftover timestamp) is
/// treated as spoken text, so ordinary sentences with a colon survive.
fn strip_speaker_label(text: &str) -> &str {
    let Some((head, tail)) = text.split_once(':') else {
        return text;
    };
    let label = head.trim();
    if label.is_empty() {
        // Text written before empty labels were handled still reads ": hello";
        // the colon belongs to the missing label, not to the spoken words.
        return tail.trim_start();
    }
    if label.chars().count() > MAX_LABEL_CHARS || label.contains('[') {
        return text;
    }
    tail.trim_start()
}

/// The `[timestamp] ` prefix of a rendered line, if present.
pub(crate) fn line_prefix(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix('[') else {
        return "";
    };
    let Some(end) = rest.find(']') else {
        return "";
    };
    // The byte after `]`, plus one ASCII space when the source has one.
    let mut len = 1 + end + 1;
    if raw[len..].starts_with(' ') {
        len += 1;
    }
    &raw[..len]
}

/// Rewrite one line in place, preserving its timestamp and changing only the
/// speaker and text.
pub(crate) fn join_line_text(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_string(),
        (_, true) => left.to_string(),
        _ => format!("{left} {right}"),
    }
}

/// The transcript text for a set of lines, newline terminated when non-empty.
pub(crate) fn lines_text(lines: &[DocLine]) -> String {
    let mut text = lines
        .iter()
        .map(|line| line.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Split `text` at `cursor`, a byte offset, snapping to the nearest word
/// boundary. Returns the two halves with their surrounding spaces trimmed, or
/// `None` when the cursor leaves one half empty.
pub(crate) fn split_words_at(text: &str, cursor: usize) -> Option<(&str, &str)> {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.len() < 2 {
        return None;
    }
    let target = text[..cursor].chars().count();
    // A split needs spoken text on both sides of the cursor.
    if target == 0 || target >= chars.len() {
        return None;
    }
    // The nearest whitespace to the caret is the word boundary the user meant.
    let mut split_at = None;
    let mut best = usize::MAX;
    for (count, (byte, ch)) in chars.iter().enumerate() {
        if ch.is_whitespace() && *byte > 0 {
            let distance = count.abs_diff(target);
            if distance < best {
                best = distance;
                split_at = Some(*byte);
            }
        }
    }
    // Scripts without spaces have no boundary to snap to, so the caret wins.
    let split_at = split_at.unwrap_or(cursor);
    let left = text[..split_at].trim_end();
    let right = text[split_at..].trim_start();
    (!left.is_empty() && !right.is_empty()).then_some((left, right))
}

/// Byte offset of a text editor caret given as `(line, column)` within `text`.
///
/// The editor reports the column as a byte index into its line; joining the
/// lines back up with `'\n'` restores the offset the caller needs.
pub(crate) fn cursor_byte_index(text: &str, (line, column): (usize, usize)) -> usize {
    let mut offset = 0;
    for (index, chunk) in text.split('\n').enumerate() {
        if index == line {
            return offset + column.min(chunk.len());
        }
        offset += chunk.len() + 1;
    }
    text.len()
}

/// Merge adjacent lines from the same speaker, preserving the first line's
/// timestamp.
pub(crate) fn join_adjacent(lines: &[DocLine]) -> Vec<DocLine> {
    let mut joined: Vec<DocLine> = Vec::new();
    for line in lines {
        if let Some(last) = joined.last_mut() {
            if !line.speaker.is_empty() && last.speaker == line.speaker {
                let text = line_text(line);
                let left = line_text(last);
                let speaker = last.speaker.clone();
                rewrite_line(last, &speaker, &join_line_text(&left, &text));
                continue;
            }
        }
        joined.push(line.clone());
    }
    joined
}

pub(crate) fn rewrite_line(line: &mut DocLine, speaker: &str, text: &str) {
    let prefix = line_prefix(&line.raw).to_string();
    line.raw = if speaker.is_empty() {
        format!("{prefix}{text}")
    } else {
        format!("{prefix}{speaker}: {text}")
    };
    line.speaker = speaker.to_string();
}

/// Confidence given to a line whose text or speaker a human has rewritten.
/// Well above the low-confidence threshold, so the line stops being flagged.
pub(crate) const HUMAN_CONFIDENCE: f32 = 1.0;

/// [`rewrite_line`] plus the human voucher: a manual edit is a person saying
/// the line is right, so its ASR confidence no longer marks it as uncertain.
pub(crate) fn rewrite_edited_line(line: &mut DocLine, speaker: &str, text: &str) {
    rewrite_line(line, speaker, text);
    line.confidence = Some(HUMAN_CONFIDENCE);
}

/// Parse "[time] Speaker: text" transcript lines into the render model.
pub(crate) fn parse_lines(text: &str) -> Vec<DocLine> {
    text.lines()
        .map(|line| DocLine {
            raw: line.to_string(),
            speaker: speaker_of(line),
            start: parse_time_prefix(line),
            confidence: None,
        })
        .collect()
}

/// Line indices sorted by start time (untimed lines last), stable.
pub(crate) fn sorted_display_order(lines: &[DocLine]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..lines.len()).collect();
    indices.sort_by(|a, b| {
        let ta = lines[*a].start.unwrap_or(f32::INFINITY);
        let tb = lines[*b].start.unwrap_or(f32::INFINITY);
        ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
    });
    indices
}

/// Distinct speaker names in transcript order.
pub(crate) fn distinct_speaker_order(lines: &[DocLine]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for line in lines {
        if !line.speaker.is_empty() && !order.contains(&line.speaker) {
            order.push(line.speaker.clone());
        }
    }
    order
}

/// Parse a leading "[mm:ss]" / "[h:mm:ss]" timestamp into seconds.
pub(crate) fn parse_time_prefix(raw: &str) -> Option<f32> {
    let inner = raw.strip_prefix('[')?;
    let end = inner.find(']')?;
    let stamp = &inner[..end];
    let mut seconds = 0.0f32;
    let mut seen = false;
    for part in stamp.split(':') {
        let value: f32 = part.trim().parse().ok()?;
        seconds = seconds * 60.0 + value;
        seen = true;
    }
    seen.then_some(seconds)
}

/// Extract the speaker label from a rendered transcript line.
pub(crate) fn speaker_of(raw: &str) -> String {
    let rest = match raw.find(']') {
        Some(index) if raw.starts_with('[') => &raw[index + 1..],
        _ => raw,
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    match rest.split_once(": ") {
        Some((head, _)) => head.trim().to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(raw: &str, speaker: &str) -> DocLine {
        DocLine {
            raw: raw.to_string(),
            speaker: speaker.to_string(),
            start: parse_time_prefix(raw),
            confidence: None,
        }
    }

    #[test]
    fn line_text_strips_a_renamed_speaker_label() {
        // The raw text still carries the old label after a speaker rename.
        let renamed = line("[00:00] ALICE: hello there", "BOB");
        assert_eq!(line_text(&renamed), "hello there");
        let current = line("[00:00] BOB: hello there", "BOB");
        assert_eq!(line_text(&current), "hello there");
        let unlabelled = line("[00:00] hello there", "");
        assert_eq!(line_text(&unlabelled), "hello there");
        // A colon deep in a sentence is not a label.
        let sentence = line(
            "[00:00] SPEAKER 1: this is a much longer opening clause: still the text",
            "SPEAKER 1",
        );
        assert!(line_text(&sentence).starts_with("this is a much longer"));
    }

    #[test]
    fn split_words_at_snaps_to_the_nearest_word_boundary() {
        let text = "hello world again";
        // Caret inside the first word snaps forward to the space after it.
        assert_eq!(split_words_at(text, 6), Some(("hello", "world again")));
        // Caret at the space itself.
        assert_eq!(split_words_at(text, 5), Some(("hello", "world again")));
        // Caret inside the last word snaps back to the space before it.
        assert_eq!(split_words_at(text, 14), Some(("hello world", "again")));
        // A caret needing a whole half on each side of it.
        assert_eq!(split_words_at(text, 0), None);
        assert_eq!(split_words_at(text, text.len()), None);
        // Text without spaces splits at the caret, as it did before.
        assert_eq!(split_words_at("word", 2), Some(("wo", "rd")));
    }

    #[test]
    fn inline_editor_holds_only_the_clicked_line() {
        let lines = vec![
            line("[00:00] A: one", "A"),
            line("[00:01] B: two", "B"),
            line("[00:02] A: three", "A"),
        ];
        for index in 0..lines.len() {
            let edit = LineEdit::inline(index, &lines).unwrap();
            assert_eq!(edit.index, index);
            assert_eq!(edit.rows.len(), 1);
            assert_eq!(edit.current_position(), 0);
            let row = edit.current_row().unwrap();
            assert_eq!(row.index, index);
            assert_eq!(row.speaker, lines[index].speaker);
            assert_eq!(
                row.content.text().trim_end(),
                line_text(&lines[index]),
                "the row carries the spoken text"
            );
        }

        assert!(LineEdit::inline(3, &lines).is_none());
        assert!(LineEdit::inline(0, &[]).is_none());
    }

    #[test]
    fn cursor_byte_index_joins_editor_lines() {
        let text = "hello\nworld\n";
        assert_eq!(cursor_byte_index(text, (0, 5)), 5);
        assert_eq!(cursor_byte_index(text, (1, 3)), 9);
        assert_eq!(cursor_byte_index(text, (9, 0)), text.len());
    }
}
