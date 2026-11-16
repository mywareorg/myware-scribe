//! Headless CLI (`myware-scribe --cli ...`).

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
    let mut preset = String::from("balanced");
    let mut num_speakers: Option<i32> = Some(2);
    let mut output_dir: Option<PathBuf> = None;
    let mut timestamps = true;
    let mut progress_json = false;
    let mut cache = true;
    let mut mode = PipelineMode::Diarized;
    // Label an ASR-only run's lone turn SPEAKER 1. The GUI sets this when the
    // user picks a single speaker, so the lines still carry a label without
    // loading the diarizer.
    let mut single_speaker = false;
    // Diagnostic: run the diarizer only and print the turn map, so a long
    // file can be analysed in minutes instead of a full transcript run.
    let mut diarize_only = false;
    // Every preset is multilingual, so default to Whisper's detection;
    // `--language <code>` pins one.
    let mut language: Option<String> = None;
    // Off by default in scripts: a CLI user asking for exactly what is in the
    // file should get exactly that. The GUI turns it on.
    let mut normalize = false;

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
            "--balanced" => preset = "balanced".to_string(),
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
            "--asr-only" => mode = PipelineMode::AsrOnly,
            "--single-speaker" => single_speaker = true,
            "--diarize-only" => diarize_only = true,
            "--no-cache" => cache = false,
            "--normalize" => normalize = true,
            "--no-normalize" => normalize = false,
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

    if !preset.eq_ignore_ascii_case("speed")
        && !preset.eq_ignore_ascii_case("balanced")
        && !preset.eq_ignore_ascii_case("accuracy")
    {
        eprintln!("unknown preset: {preset} (use speed, balanced or accuracy)");
        return 2;
    }
    if inputs.is_empty() {
        eprintln!("usage: myware-scribe --cli <audio>... [--preset speed|balanced|accuracy] [--language <code>] [--asr-only] [--single-speaker] [--progress-json] [--normalize]");
        return 2;
    }

    // The CLI shares the GUI's entitlement gate: the Store build must not be
    // unlockable by adding `--cli --preset accuracy`.
    let now = SystemTime::now();
    let license_file =
        license::license_path().unwrap_or_else(|| std::path::PathBuf::from("license.json"));
    let mut license_state = license::State::load(&license_file, now);
    license_state.refresh(None, now);
    // Keep the clock floor current so a CLI-only user cannot roll the clock back
    // between runs and stretch the offline grace window.
    license_state.save(&license_file);
    let tier = license_state.access.tier;

    if tier == Tier::Free {
        let preset_feature = if preset.eq_ignore_ascii_case("balanced") {
            Some(Feature::BalancedPreset)
        } else if preset.eq_ignore_ascii_case("accuracy") {
            Some(Feature::AccuracyPreset)
        } else {
            None
        };
        if let Some(feature) = preset_feature {
            eprintln!(
                "{} is a Pro feature. Use --preset speed, or buy Pro in the app.",
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
    if diarize_only {
        return diarize_only_run(&inputs, num_speakers);
    }

    // A single speaker never needs diarization: same fast path as the GUI.
    // `--single-speaker` also asks for a label on that fast path.
    if single_speaker {
        mode = PipelineMode::AsrOnly;
    } else if mode == PipelineMode::Diarized {
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
        single_speaker,
        normalize,
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

/// Print the diarization turn map for one file as TSV: `start\tend\tspeaker`.
///
/// The turns go through the same cleanup the pipeline applies (`clean_turns`),
/// so a scored baseline describes the shipped turn geometry rather than the
/// raw model output. A leading `#` line carries the timing and the cleanup
/// counts; `diarize-score.py` treats it as a comment.
fn diarize_only_run(inputs: &[std::path::PathBuf], num_speakers: Option<i32>) -> i32 {
    let Some(input) = inputs.first() else {
        eprintln!("usage: myware-scribe --cli <audio> --diarize-only");
        return 1;
    };
    let (samples, rate) = match crate::audio::decode(input) {
        Ok(decoded) => decoded,
        Err(message) => {
            eprintln!("error: {message}");
            return 1;
        }
    };
    let audio_seconds = if rate > 0 {
        samples.len() as f64 / rate as f64
    } else {
        0.0
    };
    let started = std::time::Instant::now();
    let raw = match crate::diarize::diarize(
        samples.as_slice(),
        &crate::models::segmentation_model(),
        &crate::models::embedding_model(),
        num_speakers,
        crate::diarize::CLUSTER_THRESHOLD,
    ) {
        Ok(turns) => turns,
        Err(message) => {
            eprintln!("error: {message}");
            return 1;
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    let raw_turns = raw.len();
    let (turns, merged, dropped) = crate::pipeline::clean_turns(raw);
    println!(
        "# diarize {elapsed:.2}s audio {audio_seconds:.1}s threads {} raw {raw_turns} \
         merged {merged} dropped {dropped} kept {} speaker(s) {}",
        crate::diarize::threads(),
        turns.len(),
        turns
            .iter()
            .map(|turn| turn.speaker.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
    );
    for turn in &turns {
        println!("{:.3}\t{:.3}\t{}", turn.start, turn.end, turn.speaker);
    }
    0
}

/// `myware-scribe --cli models [download] [--force] [--verify]`
/// `myware-scribe --cli models notice` prints model attribution.
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

/// `myware-scribe --cli probe <audio> <buckets>`: duration + RMS waveform.
fn run_probe(args: &[String]) -> i32 {
    let path = match args.first() {
        Some(path) => Path::new(path),
        None => {
            eprintln!("usage: myware-scribe --cli probe <audio> <buckets>");
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
