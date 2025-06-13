#![feature(unix_mkfifo, path_file_prefix, map_try_insert, if_let_guard)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Write, stdout};
use std::panic;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use log::{Level, Log, debug, error, info, trace, warn};
use log_reload::ReloadLog;
use piper_rs::synth::{AudioOutputConfig, PiperSpeechSynthesizer};
use piper_rs::{ModelConfig, PiperError};
use serde_ssml::SsmlElement;
use xdg::BaseDirectories;

mod io;

fn main() -> Result<()> {
    if let Err(e) = start() {
        error!("{e:?}");
        for e in e.chain() {
            send!("300-{e}");
        }
        send!("300 MODULE ERROR");
        Err(e)
    } else {
        Ok(())
    }
}

fn setup_logger(path: Option<&str>) -> Result<Box<(dyn Log + 'static)>> {
    let mut dispatch = fern::Dispatch::new()
        .format(|out, message, record| out.finish(format_args!("[{}] {message}", record.level())))
        .chain(std::io::stderr());
    if let Some(path) = path {
        dispatch = dispatch.chain(fern::log_file(path).context("Error opening log file")?);
    }
    Ok(dispatch.into_log().1)
}

fn start() -> Result<()> {
    let log_handle = {
        let (path, level) = if cfg!(debug_assertions) {
            (Some("/tmp/piper-speechd.log"), Level::Trace)
        } else {
            (None, Level::Warn)
        };

        // SAFETY: safe on release because an error can only occur when path is Some
        let inner = setup_logger(path).unwrap();
        let logger = ReloadLog::new(log_reload::LevelFilter::new(level, inner));
        let handle = logger.handle();
        // SAFETY: and because this is the first time the logger is being set
        log::set_max_level(log::LevelFilter::Trace);
        log::set_boxed_logger(Box::new(logger)).unwrap();
        info!("Logging initialized");
        handle
    };

    panic::set_hook(Box::new(|info| {
        error!("{info}");
    }));

    if recv!() != "INIT" {
        bail!("Server did not start with INIT!");
    }

    send!("299-Everything ok so far");

    let Some(voice_dir) = BaseDirectories::new()
        .get_data_home()
        .map(|dir| dir.join("piper-voices"))
    else {
        bail!("Failed to resolve voice directory. XDG_DATA_HOME and HOME are unset");
    };

    let mut voices: HashMap<String, (PathBuf, Option<PiperSpeechSynthesizer>)> = {
        voice_dir
            .read_dir()
            .context("Failed to enumerate voices")?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = if entry.file_type().ok()?.is_dir() {
                    let mut name = entry.file_name();
                    name.push(".json");

                    let path = entry.path().join(name);
                    if !path.exists() {
                        return None;
                    }
                    path
                } else if entry.path().extension() == Some(OsStr::new("json")) {
                    entry.path()
                } else {
                    return None;
                };

                let file = match File::open(&path) {
                    Ok(file) => file,
                    Err(e) => {
                        warn!("Failed to open model config {path:?}: {e:?}");
                        return None;
                    }
                };
                let config: ModelConfig = match serde_json::from_reader(file) {
                    Ok(config) => config,
                    Err(e) => {
                        warn!("Failed to parse model config: {path:?}: {e:?}");
                        return None;
                    }
                };

                // SAFETY: safe because we matched on having a name ending in .json
                let mut name = path.file_prefix().unwrap().to_string_lossy().to_string();

                // strip the lang prefix from the name if there is one
                if name
                    .to_lowercase()
                    .replace('_', "-")
                    .strip_prefix(&config.espeak.voice)
                    .is_some_and(|name| name.starts_with(['-', '_']))
                {
                    name = name[config.espeak.voice.len() + 1..].to_string()
                }

                Some((name, (path.clone(), None)))
            })
            .collect()
    };

    let Some(mut voice) = voices.keys().next().map(String::to_string) else {
        bail!("No models available");
    };

    let mut pitch = 1.0;
    let mut rate = 1.0;
    let mut volume = 1.0;

    send!("299 OK LOADED SUCCESSFULLY");

    loop {
        match recv!().as_str() {
            "AUDIO" => {
                send!("207 OK RECEIVING AUDIO SETTINGS");
                if recv!() != "audio_output_method=server" || recv!() != "." {
                    bail!("Audio output method must be server!");
                }
                send!("203 OK AUDIO INITIALIZED");
            }

            "LOGLEVEL" => {
                send!("207 OK RECEIVING LOGLEVEL SETTINGS");
                loop {
                    match recv!().as_str() {
                        "." => break,
                        line if let Some((key, value)) = line.split_once('=') => {
                            if key == "log_level" {
                                let level = value
                                    .parse::<usize>()
                                    .context("Invalid value for log_level")?;
                                let level = match level {
                                    1 => Level::Error,
                                    2 => Level::Warn,
                                    3 => Level::Info,
                                    4 => Level::Debug,
                                    5 => Level::Trace,
                                    _ => bail!("Invalid value for log_level"),
                                };

                                // SAFETY: safe since the LevelFilter is owned by the ReloadLog
                                log_handle
                                    .modify(|level_filter| level_filter.set_level(level))
                                    .unwrap();
                            } else {
                                warn!("Ignoring unknown setting {key:?}");
                            }
                        }
                        _ => {
                            bail!("Malformed setting");
                        }
                    }
                }
                send!("203 OK LOGLEVEL SET");
            }

            "LIST VOICES" => {
                for (name, (path, _)) in &voices {
                    let file = match File::open(path) {
                        Ok(file) => file,
                        Err(e) => {
                            warn!("Failed to open model config {path:?}: {e:?}");
                            continue;
                        }
                    };
                    let config: ModelConfig = match serde_json::from_reader(file) {
                        Ok(config) => config,
                        Err(e) => {
                            warn!("Failed to parse model config: {path:?}: {e:?}");
                            continue;
                        }
                    };

                    // intentionally make it so firefox doesn't recognize the format
                    // otherwise it'll hide the name, which isn't what we want
                    let lang = {
                        let Some((lang, region)) = config.espeak.voice.split_once('-') else {
                            warn!("Malformed espeak voice in config {path:?}");
                            continue;
                        };
                        [lang, &region.to_uppercase()].join("_")
                    };

                    send!(
                        "200-{name}\t{lang}\t{}",
                        config.dataset.unwrap_or("none".to_string())
                    );
                }
                send!("200 OK VOICE LIST SENT");
            }

            "SET" => {
                send!("203 OK RECEIVING SETTINGS");
                loop {
                    let line = recv!();
                    if line == "." {
                        break;
                    }

                    // pitch, rate, and volume are all ±100
                    let Some((name, value)) = line.split_once('=') else {
                        send!("Ignoring improperly formatted keypair {line:?}");
                        continue;
                    };
                    match name {
                        "pitch" => {
                            const NORMAL_PITCH: f32 = 1.0;
                            const MIN_PITCH: f32 = 0.5;
                            const MAX_PITCH: f32 = 2.0;

                            pitch =
                                value.parse::<f32>().context("Invalid value for pitch")? / 100.0;

                            if pitch < 0.0 {
                                pitch = NORMAL_PITCH + (NORMAL_PITCH - MIN_PITCH) * pitch;
                            } else {
                                pitch = NORMAL_PITCH + (MAX_PITCH - NORMAL_PITCH) * pitch;
                            }
                        }

                        // adapted from https://github.com/brailcom/speechd/blob/ffbbec5aa1b53cca96b2dbb42c54d520ef1cf098/src/modules/espeak.c#L416
                        "rate" => {
                            const NORMAL_RATE: f32 = 1.0;
                            const MIN_RATE: f32 = 0.5;
                            const MAX_RATE: f32 = 4.5;

                            rate = value.parse::<f32>().context("Invalid value for rate")? / 100.0;

                            if rate < 0.0 {
                                rate = NORMAL_RATE + (NORMAL_RATE - MIN_RATE) * rate;
                            } else {
                                rate = NORMAL_RATE + (MAX_RATE - NORMAL_RATE) * rate;
                            }
                        }

                        "volume" => {
                            volume =
                                value.parse::<f32>().context("Invalid value for volume")? / 100.0;
                        }

                        "synthesis_voice" => {
                            if voices.contains_key(value) {
                                voice = value.to_string();
                            } else {
                                warn!("Not setting voice to unknown {value:?}");
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
                let ssml = match serde_ssml::from_str(buf) {
                    Ok(ssml) => ssml,
                    Err(errors) => {
                        return errors
                            .into_iter()
                            .fold(Err(anyhow!("SSML parsing failed")), Result::context);
                    }
                };
                debug!("Parsed SSML: {ssml:#?}");
                send!("200 OK SPEAKING");
                send!("701 BEGIN");
                match speak(&ssml.elements, &mut voices, &voice, pitch, rate, volume) {
                    Ok(StopCondition::End | StopCondition::Pause { .. }) => {
                        send!("702 END");
                    }

                    Ok(StopCondition::Stop) => {
                        send!("703 STOP");
                    }

                    Err(error) => {
                        error!("{error:?}");
                        send!("703-{error:?}");
                        send!("703 STOP");
                    }
                }
            }

            line if let Some(path) = line.strip_prefix("DEBUG ON ") => {
                let logger = setup_logger(Some(path))?;
                log_handle
                    .modify(|level_filter| {
                        level_filter.set_inner(Box::new(logger));
                    })
                    // SAFETY: safe since the LevelFilter is owned by the ReloadLog
                    .unwrap();
                info!("Set log path to {path}");
                send!("200 OK DEBUGGING ON");
            }

            _ => send!("300 ERR UNKNOWN COMMAND"),
        }
    }
}

enum StopCondition {
    End,
    Stop,
    Pause { handled: bool },
}

fn speak(
    elements: &[SsmlElement],
    voices: &mut HashMap<String, (PathBuf, Option<PiperSpeechSynthesizer>)>,
    voice: &str,
    pitch: f32,
    rate: f32,
    volume: f32,
) -> Result<StopCondition> {
    let mut should_pause = false;

    for element in elements {
        match element {
            SsmlElement::Speak { children, .. } => {
                match speak(children, voices, voice, pitch, rate, volume)? {
                    StopCondition::End => (),
                    StopCondition::Stop => return Ok(StopCondition::Stop),
                    StopCondition::Pause { handled: true } => {
                        return Ok(StopCondition::Pause { handled: true });
                    }
                    StopCondition::Pause { handled: false } => {
                        should_pause = true;
                    }
                }
            }

            SsmlElement::Text(text) => {
                let synth = match &voices[voice].1 {
                    Some(synth) => synth,
                    None => {
                        let model = piper_rs::from_config_path(&voices[voice].0)
                            .context("Failed to parse model config")?;
                        let synth = PiperSpeechSynthesizer::new(model)
                            .context("Failed to initialize model")?;
                        // SAFETY: safe as long as voice is a valid key to voices
                        // we only set it if it is a valid key, so it should be guaranteed to be
                        // also, if there aren't any voices, we already panicked
                        voices.get_mut(voice).unwrap().1.insert(synth)
                    }
                };

                let model = synth.clone_model();
                let output_info = model.audio_output_info();

                let output_config = Some(AudioOutputConfig {
                    rate: Some(rate),
                    volume: Some(volume),
                    pitch: Some(pitch),
                    appended_silence_ms: None,
                });

                let output: &mut dyn Iterator<Item = Result<Vec<u8>, PiperError>> = if model
                    .supports_streaming_output()
                {
                    &mut synth
                        .synthesize_streamed(text.to_string(), output_config, 1, 1)?
                        .map(|audio| Ok(audio?.as_wave_bytes()))
                } else {
                    &mut synth
                        .synthesize_parallel(text.to_string(), output_config)?
                        .map(|audio| -> Result<Vec<u8>, PiperError> { Ok(audio?.as_wave_bytes()) })
                };
                for audio in output {
                    // handle interrupts
                    if let Some(line) = try_recv!() {
                        match line.as_str() {
                            "STOP" => return Ok(StopCondition::Stop),
                            "PAUSE" => should_pause = true,
                            cmd => bail!("Unexpected command during playback: {cmd:?}"),
                        }
                    }

                    let mut audio = audio?;

                    send!("705-bits={}", output_info.sample_width * 8);
                    send!("705-num_channels={}", output_info.num_channels);
                    send!("705-sample_rate={}", output_info.sample_rate);
                    send!("705-num_samples={}", audio.len() / output_info.sample_width);

                    for i in (0..audio.len()).rev() {
                        if audio[i] == b'\n' || audio[i] == 0x7d {
                            audio[i] ^= 1 << 5;
                            audio.insert(i, 0x7d);
                        }
                    }

                    print!("705-AUDIO\0");
                    stdout().write_all(&audio)?;
                    send!();
                    trace!("< 705-AUDIO<raw audio bytes...>");
                    send!("705 AUDIO");
                }
            }

            SsmlElement::Mark { name } => {
                send!("700-{name}");
                send!("700 INDEX MARK");
                if should_pause {
                    return Ok(StopCondition::Pause { handled: true });
                }
            }

            _ => unimplemented!(),
        }
    }

    if should_pause {
        Ok(StopCondition::Pause { handled: false })
    } else {
        Ok(StopCondition::End)
    }
}
