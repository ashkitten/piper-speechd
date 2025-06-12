#![feature(unix_mkfifo, path_file_prefix, map_try_insert)]

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Write, stdout};
use std::{env, panic};
use std::{fs::OpenOptions, io::stdin};

use libc::{F_GETFL, F_SETFL, O_NONBLOCK, STDIN_FILENO, fcntl};
use piper_rs::ModelConfig;
use piper_rs::synth::{AudioOutputConfig, PiperSpeechSynthesizer};
use serde_ssml::SsmlElement;
use xdg::BaseDirectories;

const LOG_PATH: &'static str = concat!("/tmp/", env!("CARGO_BIN_NAME"), ".log");

macro_rules! log {
    () => {
        writeln!(
            OpenOptions::new()
                .write(true)
                .create(true)
                .append(true)
                .open(LOG_PATH)
                .unwrap(),
        ).unwrap()
    };

    ($($arg:tt)*) => {{
        writeln!(
            OpenOptions::new()
                .write(true)
                .create(true)
                .append(true)
                .open(LOG_PATH)
                .unwrap(),
            $($arg)*
        ).unwrap()
    }};
}

macro_rules! send {
    () => {
        println!()
    };

    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        log!("< {msg}");
        println!("{msg}");
    }};
}

macro_rules! recv {
    () => {{
        let msg = stdin().lines().next().unwrap().unwrap();
        log!("> {msg}");
        msg
    }};
}

fn main() {
    panic::set_hook(Box::new(|info| {
        log!("{}", info);
    }));

    let voice_dir = BaseDirectories::new()
        .get_data_home()
        .unwrap()
        .join("piper-voices");

    let mut voices: HashMap<String, PiperSpeechSynthesizer> = HashMap::new();

    let mut voice = {
        let path = voice_dir
            .read_dir()
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.path().extension() == Some(OsStr::new("json")))
            })
            .next()
            .unwrap()
            .unwrap()
            .path();

        let voice = path.file_prefix().unwrap().to_str().unwrap().to_string();

        let _ = voices.try_insert(
            voice.clone(),
            PiperSpeechSynthesizer::new(piper_rs::from_config_path(&path).unwrap()).unwrap(),
        );

        voice
    };

    let mut pitch = 1.0;
    let mut rate = 1.0;
    let mut volume = 1.0;

    loop {
        match recv!().as_str() {
            "INIT" => {
                send!("299-Everything ok so far.");
                send!("299 OK LOADED SUCCESSFULLY");
            }

            "AUDIO" => {
                send!("207 OK RECEIVING AUDIO SETTINGS");
                assert_eq!(recv!(), "audio_output_method=server");
                assert_eq!(recv!(), ".");
                send!("203 OK AUDIO INITIALIZED");
            }

            "LOGLEVEL" => {
                send!("207 OK RECEIVING LOGLEVEL SETTINGS");
                // TODO: support loglevel
                while recv!() != "." {}
                send!("203 OK LOGLEVEL SET");
            }

            "LIST VOICES" => {
                for entry in voice_dir.read_dir().unwrap() {
                    let Ok(entry) = entry else {
                        continue;
                    };

                    if entry.path().extension() != Some(OsStr::new("json")) {
                        continue;
                    }

                    let config: ModelConfig =
                        serde_json::from_reader(File::open(entry.path()).unwrap()).unwrap();

                    send!(
                        "200-{}\t{}\t{}-{}",
                        entry.path().file_prefix().unwrap().to_str().unwrap(),
                        config.language.as_ref().unwrap().code,
                        config.dataset.unwrap(),
                        config.audio.quality.unwrap(),
                    );
                }
                send!("200 OK VOICE LIST SENT");
            }

            "SET" => {
                send!("203 OK RECEIVING SETTINGS");
                // TODO: support settings
                loop {
                    let line = recv!();
                    if line == "." {
                        break;
                    }
                    let (name, value) = line.split_once('=').unwrap();
                    match name {
                        "pitch" => {
                            pitch = value.parse::<f32>().unwrap() + 1.0;
                        }
                        "rate" => {
                            rate = value.parse::<f32>().unwrap() / 100.0 + 1.0;
                        }
                        "volume" => {
                            volume = value.parse::<f32>().unwrap() / 100.0;
                        }
                        "synthesis_voice" => {
                            let path = voice_dir.join(format!("{value}.onnx.json"));
                            if path.exists() {
                                let _ = voices.try_insert(
                                    value.to_string(),
                                    PiperSpeechSynthesizer::new(
                                        piper_rs::from_config_path(&path).unwrap(),
                                    )
                                    .unwrap(),
                                );

                                voice = value.to_string();
                            }
                        }
                        _ => (),
                    }
                }
                send!("203 OK SETTINGS RECEIVED");
            }

            "SPEAK" => {
                send!("202 OK RECEIVING MESSAGE");
                let mut buf = String::new();
                loop {
                    let line = recv!();
                    if line == "." {
                        break;
                    }
                    buf += &line;
                    buf += "\n";
                }
                let ssml = serde_ssml::from_str(buf).unwrap();
                log!("Parsed SSML: {:#?}", ssml);
                send!("200 OK SPEAKING");
                send!("701 BEGIN");
                if let Err(error) = speak(&ssml.elements, &mut voices, &voice, pitch, rate, volume)
                {
                    log!("Error: {:?}", error);
                    send!("703 STOP");
                } else {
                    send!("702 END");
                }
            }

            "STOP" => {}

            _ => (),
        }
    }
}

fn speak(
    elements: &[SsmlElement],
    voices: &mut HashMap<String, PiperSpeechSynthesizer>,
    voice: &str,
    pitch: f32,
    rate: f32,
    volume: f32,
) -> Result<(), Box<dyn Error>> {
    for element in elements {
        handle_interrupts()?;

        match element {
            SsmlElement::Speak {
                version,
                xmlns,
                lang,
                children,
            } => {
                speak(children, voices, voice, pitch, rate, volume)?;
            }

            SsmlElement::Text(text) => {
                let output = voices[voice].synthesize_parallel(
                    text.to_string(),
                    Some(AudioOutputConfig {
                        rate: Some(rate),
                        volume: Some(volume),
                        pitch: Some(pitch),
                        appended_silence_ms: None,
                    }),
                )?;
                for audio in output {
                    handle_interrupts()?;

                    let audio = audio?;
                    send!("705-bits={}", audio.info.sample_width * 8);
                    send!("705-num_channels={}", audio.info.num_channels);
                    send!("705-sample_rate={}", audio.info.sample_rate);
                    send!("705-num_samples={}", audio.samples.len());

                    let mut encoded = audio.as_wave_bytes();
                    for i in (0..encoded.len()).rev() {
                        if encoded[i] == '\n' as u8 || encoded[i] == 0x7d {
                            encoded[i] ^= 1 << 5;
                            encoded.insert(i, 0x7d);
                        }
                    }

                    print!("705-AUDIO\0");
                    stdout().write(&encoded)?;
                    println!();
                    log!("< 705-AUDIO<raw audio bytes...>");
                    send!("705 AUDIO");
                }
            }

            SsmlElement::Mark { name } => {
                send!("700-{}", name);
                send!("700 INDEX MARK");
            }

            _ => unimplemented!(),
        }
    }
    Ok(())
}

fn handle_interrupts() -> Result<(), Box<dyn Error>> {
    let flags = unsafe { fcntl(STDIN_FILENO, F_GETFL) };
    unsafe { fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) };

    let mut buf = String::new();
    match stdin().read_line(&mut buf) {
        Ok(0) => panic!("stdin closed unexpectedly"),
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            // return stdin to blocking
            unsafe { fcntl(STDIN_FILENO, F_SETFL, flags) };
            Ok(())
        }
        Err(e) => Err(e).unwrap(),
        Ok(_) => {
            assert_eq!(buf.pop(), Some('\n'));
            log!("> {}", buf);
            match buf.as_str() {
                "STOP" => {
                    // return stdin to blocking
                    unsafe { fcntl(STDIN_FILENO, F_SETFL, flags) };
                    Err("stop requested".into())
                }

                "PAUSE" => {
                    // return stdin to blocking
                    unsafe { fcntl(STDIN_FILENO, F_SETFL, flags) };

                    match recv!().as_str() {
                        "RESUME" => Ok(()),
                        "STOP" => Err("stop requested".into()),
                        _ => unimplemented!("unexpected command during pause: {:?}", buf),
                    }
                }

                _ => unimplemented!("unexpected command during playback: {:?}", buf),
            }
        }
    }
}
