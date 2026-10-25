//! Headless CLI (`hippo-transcribe --cli ...`).

use std::path::{Path, PathBuf};

use crate::audio;
use crate::event::Event;
use std::time::SystemTime;

use crate::license::{self, Feature, Tier};
use crate::pipeline::{self, PipelineMode, PipelineOptions};

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("models") => return run_models(&args[1..]),
        Some("probe") => return run_probe(&args[1..]),
        _ => {}
    }

    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut preset = String::from("speed");
    let mut num_speakers: Option<i32> = Some(2);
    let mut output_dir: Option<PathBuf> = None;
    let mut timestamps = true;
    let mut progress_json = false;
    let mut cache = true;
    let mut mode = PipelineMode::Diarized;
    // Every preset is multilingual, so default to Whisper's detection;
    // `--language <code>` pins one.
    let mut language: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--preset" => {
                i += 1;
                if let Some(value) = args.get(i) {
                    preset = value.clone();
                }
            }
            "--speed" => preset = "speed".to_string(),
            "--accuracy" => preset = "accuracy".to_string(),
            "--num-speakers" => {
                i += 1;
                num_speakers = args.get(i).and_then(|v| v.parse().ok());
            }

            "--output-dir" => {
                i += 1;
                output_dir = args.get(i).map(PathBuf::from);
            }
            "--no-timestamps" => timestamps = false,
            "--progress-json" => progress_json = true,
            "--no-cache" => cache = false,
            "--language" => {
                i += 1;
                let value = args.get(i).map(|value| value.trim()).unwrap_or("");
                if value.is_empty() || value.eq_ignore_ascii_case("auto") {
                    language = None;
                } else if value.len() <= 16
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    language = Some(value.to_string());
                } else {
                    eprintln!("invalid language code: {value}");
                    return 2;
                }
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                return 2;
            }
            other => inputs.push(PathBuf::from(other)),
        }
        i += 1;
    }

    if !preset.eq_ignore_ascii_case("speed") && !preset.eq_ignore_ascii_case("accuracy") {
        eprintln!("unknown preset: {preset} (use speed or accuracy)");
        return 2;
    }
    if inputs.is_empty() {
        eprintln!("usage: hippo-transcribe --cli <audio>... [--preset speed|accuracy] [--language <code>] [--progress-json]");
        return 2;
    }

    // The CLI shares the GUI's entitlement gate: the Store build must not be
    // unlockable by adding `--cli --preset accuracy`.
    let now = SystemTime::now();
    let provider = license::provider();
    let mut license_state = license::State::load(
        &license::license_path().unwrap_or_else(|| std::path::PathBuf::from("license.json")),
        now,
    );
    license_state.refresh(provider.override_tier(), now);
    let tier = license_state.access.tier;

    if tier == Tier::Free {
        let preset_feature = if preset.eq_ignore_ascii_case("speed") {
            None
        } else {
            Some(Feature::AccuracyPreset)
        };
        if let Some(feature) = preset_feature {
            eprintln!(
                "{} is a Pro feature. Use --preset speed, or start the trial / buy Pro in the app.",
                feature.label()
            );
            return 1;
        }
        if inputs.len() > 1 {
            eprintln!(
                "{} is a Pro feature. Transcribe one file per run on the free tier.",
                Feature::Batch.label()
            );
            return 1;
        }
        mode = PipelineMode::AsrOnly;
    }
    // A single speaker never needs diarization: same fast path as the GUI.
    if mode == PipelineMode::Diarized {
        mode = pipeline::route(num_speakers);
    }

    let opts = PipelineOptions {
        inputs,
        output_dir,
        preset,
        num_speakers,
        timestamps,
        cache,
        language,
        mode,
        cancel: None,
    };

    let mut code = 0;
    pipeline::run(&opts, &mut |event| {
        match &event {
            Event::Error { message } => {
                eprintln!("error: {message}");
                code = 1;
            }
            Event::Log { message } => {
                if progress_json {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
            }
            Event::Done { txt, .. } => {
                if !progress_json && !txt.is_empty() {
                    println!("{txt}");
                }
            }
            _ => {}
        }
        if progress_json {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
        }
    });
    code
}

/// `hippo-transcribe --cli models [download] [--force] [--verify]`
/// `hippo-transcribe --cli models notice` prints model attribution.
///
/// Without a subcommand this verifies the installed files.
fn run_models(args: &[String]) -> i32 {
    let mut download = false;
    let mut force = false;
    let mut verify = false;
    let mut notice = false;

    for arg in args {
        match arg.as_str() {
            "download" => download = true,
            "notice" => notice = true,
            "--force" => force = true,
            "--verify" => verify = true,
            other => {
                eprintln!("unknown option: {other}");
                return 2;
            }
        }
    }

    if notice {
        print!("{}", crate::download::notice());
        return 0;
    }
    if download {
        crate::download::download(force, verify)
    } else {
        crate::download::verify_all()
    }
}

/// `hippo-transcribe --cli probe <audio> <buckets>`: duration + RMS waveform.
fn run_probe(args: &[String]) -> i32 {
    let path = match args.first() {
        Some(path) => Path::new(path),
        None => {
            eprintln!("usage: hippo-transcribe --cli probe <audio> <buckets>");
            return 2;
        }
    };
    let buckets = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(120);
    match audio::probe(path, buckets) {
        Ok((duration, rms)) => {
            println!("{duration}");
            println!(
                "{}",
                rms.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            0
        }
        Err(message) => {
            eprintln!("error: {message}");
            1
        }
    }
}
