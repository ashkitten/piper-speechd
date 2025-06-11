#![feature(unix_mkfifo)]

use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt, mkfifo},
    path::PathBuf,
};

use fork::Fork;
use piper_rs::synth::{AudioOutputConfig, PiperSpeechSynthesizer};
use rodio::buffer::SamplesBuffer;
use serde::{Deserialize, Serialize};
use xdg::BaseDirectories;

#[derive(Serialize, Deserialize, Debug)]
struct Data {
    text: String,
    model: String,
    rate: f32,
    pitch: f32,
    volume: f32,
}

fn main() {
    let mut args = std::env::args();
    let _arg0 = args.next().unwrap();

    let Some(data) = (|| {
        Some(Data {
            text: args.next()?.replace('\n', " "),
            model: args.next()?,
            rate: args.next()?.parse().ok()?,
            pitch: args.next()?.parse().ok()?,
            volume: args.next()?.parse().ok()?,
        })
    })() else {
        eprintln!(
            "usage: {} <text> <model> <rate> <pitch> <volume>",
            env!("CARGO_BIN_NAME")
        );
        return;
    };

    let work_dir = BaseDirectories::new()
        .get_runtime_directory()
        .unwrap()
        .join(env!("CARGO_BIN_NAME"));
    fs::create_dir_all(&work_dir).unwrap();

    let fifo = work_dir.join("input.fifo");
    let pidfile = work_dir.join(concat!(env!("CARGO_BIN_NAME"), ".pid"));

    if !fs::metadata(&fifo).is_ok_and(|metadata| metadata.file_type().is_fifo()) {
        mkfifo(&fifo, Permissions::from_mode(0o600)).unwrap();
    }

    if !fs::read_to_string(&pidfile).is_ok_and(|s| {
        fs::read_link(format!("/proc/{s}/exe"))
            .is_ok_and(|c| c == fs::read_link(format!("/proc/{}/exe", std::process::id())).unwrap())
    }) {
        if let Ok(Fork::Child) = fork::daemon(false, true) {
            fs::write(&pidfile, std::process::id().to_string()).unwrap();
            start_piper(fifo);
            return;
        }
    }

    let mut fifo = OpenOptions::new().write(true).open(fifo).unwrap();
    serde_json::to_writer(&mut fifo, &data).unwrap();
    fifo.write("\n".as_bytes()).unwrap();
}

fn start_piper(fifo: PathBuf) {
    let voice_dir = BaseDirectories::new()
        .get_data_home()
        .unwrap()
        .join("piper-voices");

    // open for reading and writing
    let fifo = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fifo)
        .unwrap();
    let fifo = BufReader::new(fifo);

    let mut model = String::new();
    let mut sample_rate = 0;
    let mut synth = None;
    let (_stream, handle) = rodio::OutputStream::try_default().unwrap();
    let sink = rodio::Sink::try_new(&handle).unwrap();

    let mut lines = fifo.lines();
    while let Some(Ok(line)) = lines.next() {
        let data: Data = serde_json::from_str(&line).unwrap();

        println!("got data to synthesize!:");
        println!("{:?}", data);

        // make sure that the first iteration can never match the initial value of model
        if data.model.is_empty() {
            continue;
        }

        // the compiler isn't smart enough to know that since `model` is initialized
        // to an empty string, and we check that `data.model` isn't empty,
        // the if condition always succeeds on the first iteration
        if data.model != model {
            model = data.model;

            let config_path = voice_dir.join(&format!("{model}.onnx.json"));
            let config: piper_rs::ModelConfig =
                serde_json::from_reader(File::open(&config_path).unwrap()).unwrap();
            sample_rate = config.audio.sample_rate;

            synth = Some(
                PiperSpeechSynthesizer::new(piper_rs::from_config_path(&config_path).unwrap())
                    .unwrap(),
            );
        }

        // safe because we always initialize synth first
        // trust me bro
        let synth = unsafe { synth.as_mut().unwrap_unchecked() };

        let output_config = AudioOutputConfig {
            rate: Some(data.rate),
            volume: Some(data.volume),
            pitch: Some(data.pitch),
            appended_silence_ms: Some(200),
        };

        let mut stream = synth
            .synthesize_parallel(data.text, Some(output_config))
            .unwrap();

        let mut samples = Vec::new();
        while let Some(Ok(audio)) = stream.next() {
            samples.extend(audio);
        }

        let buf = SamplesBuffer::new(1, sample_rate, samples);
        sink.append(buf);

        sink.sleep_until_end();
    }
}
