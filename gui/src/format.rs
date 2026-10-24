pub(crate) fn capitalize_first(text: &str) -> String {
    match text.chars().next() {
        Some(first) => first.to_uppercase().collect::<String>() + &text[first.len_utf8()..],
        None => String::new(),
    }
}

/// Human-readable name for a model file's internal label.
pub(crate) fn friendly_model(label: &str) -> &'static str {
    match label {
        "segmentation" => "speaker segmentation",
        "embedding" => "speaker embedding",
        "whisper-speed" => "Whisper speed model",
        "whisper-balanced" => "Whisper balanced model",
        "whisper-accuracy" => "Whisper accuracy model",
        _ => "model",
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.0} MB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Truncate the end of `text`, keeping it within `max` characters.
pub(crate) fn ellipsize(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// Truncate the middle of `text`, so a file extension or path tail survives.
pub(crate) fn ellipsize_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string();
    }
    let tail = (max / 3).min(count - 1);
    let head = max.saturating_sub(tail + 1);
    let mut out: String = text.chars().take(head).collect();
    out.push('\u{2026}');
    out.extend(text.chars().skip(count - tail));
    out
}

pub(crate) fn format_secs(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Bracketed absolute timestamp matching the backend output, e.g. "[03:07]".
pub(crate) fn format_timestamp(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("[{hours:02}:{minutes:02}:{seconds:02}]")
    } else {
        format!("[{minutes:02}:{seconds:02}]")
    }
}
