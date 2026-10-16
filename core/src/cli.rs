//! Headless CLI (`transiel --cli ...`).

use std::path::PathBuf;

use crate::event::Event;
use crate::pipeline::{self, PipelineOptions};

pub fn run(args: &[String]) -> i32 {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut preset = String::from("speed");
    let mut num_speakers: Option<i32> = Some(2);
    let mut output_dir: Option<PathBuf> = None;
    let mut timestamps = true;
    let mut join_speaker = false;
    let mut progress_json = false;
    let mut cache = true;

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
            "--accuracy" => preset = "balanced".to_string(),
            "--num-speakers" => {
                i += 1;
                num_speakers = args.get(i).and_then(|v| v.parse().ok());
            }
            "--auto-speakers" => num_speakers = None,
            "--output-dir" => {
                i += 1;
                output_dir = args.get(i).map(PathBuf::from);
            }
            "--no-timestamps" => timestamps = false,
            "--join-speaker" => join_speaker = true,
            "--progress-json" => progress_json = true,
            "--no-cache" => cache = false,
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                return 2;
            }
            other => inputs.push(PathBuf::from(other)),
        }
        i += 1;
    }

    if inputs.is_empty() {
        eprintln!("usage: transiel --cli <audio>... [--preset speed|balanced] [--progress-json]");
        return 2;
    }

    let opts = PipelineOptions {
        inputs,
        output_dir,
        preset,
        num_speakers,
        timestamps,
        join_speaker,
        cache,
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
