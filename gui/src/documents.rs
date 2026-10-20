use iced::widget::text_editor;

/// A single rendered transcript line.
#[derive(Debug, Clone)]
pub(crate) struct DocLine {
    pub(crate) raw: String,
    pub(crate) speaker: String,
    pub(crate) start: Option<f32>,
}

/// Right-click line editor: the line index plus its editable fields.
pub(crate) struct LineEdit {
    pub(crate) index: usize,
    pub(crate) speaker: String,
    pub(crate) content: text_editor::Content,
}

/// The spoken text of a rendered line, without its timestamp/speaker prefix.
pub(crate) fn line_text(line: &DocLine) -> String {
    let mut rest = line.raw.as_str();
    if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            rest = stripped[end + 1..].trim_start();
        }
    }
    if !line.speaker.is_empty() {
        if let Some(after) = rest.strip_prefix(&format!("{}: ", line.speaker)) {
            return after.to_string();
        }
        if let Some(after) = rest.strip_prefix(&format!("{}:", line.speaker)) {
            return after.trim_start().to_string();
        }
    }
    rest.to_string()
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

pub(crate) fn rewrite_line(line: &mut DocLine, speaker: &str, text: &str) {
    let prefix = line_prefix(&line.raw).to_string();
    line.raw = if speaker.is_empty() {
        format!("{prefix}{text}")
    } else {
        format!("{prefix}{speaker}: {text}")
    };
    line.speaker = speaker.to_string();
}

/// Parse "[time] Speaker: text" transcript lines into the render model.
pub(crate) fn parse_lines(text: &str) -> Vec<DocLine> {
    text.lines()
        .map(|line| DocLine {
            raw: line.to_string(),
            speaker: speaker_of(line),
            start: parse_time_prefix(line),
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
