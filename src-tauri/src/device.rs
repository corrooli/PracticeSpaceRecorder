//! The real audio hardware, via cpal. Kept as thin as possible: everything that
//! can be tested without a sound card lives in `audio.rs`. This file is excluded
//! from the coverage gate because CI machines have no inputs to open.

use crate::audio::{push_input, Backend, DeviceInfo, Input};
use crate::config::Preset;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub struct CpalBackend;

impl Backend for CpalBackend {
    fn devices(&self) -> Vec<DeviceInfo> {
        list_devices()
    }
    fn open(&self, preset: &Preset) -> Result<Input, String> {
        open_stream(preset)
    }
}

fn list_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let mut channels = 0u16;
            let mut rates = Vec::new();
            if let Ok(cfgs) = d.supported_input_configs() {
                for c in cfgs {
                    channels = channels.max(c.channels());
                    for r in [44100u32, 48000, 88200, 96000, 176400, 192000] {
                        if c.min_sample_rate().0 <= r && r <= c.max_sample_rate().0 && !rates.contains(&r) {
                            rates.push(r);
                        }
                    }
                }
            }
            if channels == 0 {
                continue;
            }
            rates.sort_unstable();
            out.push(DeviceInfo { is_default: name == default_name, name, channels, rates });
        }
    }
    out
}


fn open_stream(p: &Preset) -> Result<Input, String> {
    let host = cpal::default_host();
    let device = if p.device.is_empty() {
        host.default_input_device().ok_or("No input device available")?
    } else {
        host.input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| n == p.device).unwrap_or(false))
            .ok_or_else(|| format!("'{}' not connected", p.device))?
    };
    let name = device.name().unwrap_or_else(|_| "Unknown device".into());
    let cfgs: Vec<_> = device.supported_input_configs().map_err(|e| e.to_string())?.collect();
    if cfgs.is_empty() {
        return Err(format!("'{name}' has no input configurations"));
    }
    let fmt_rank = |f: cpal::SampleFormat| match f {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::I32 => 1,
        cpal::SampleFormat::I16 => 2,
        _ => 3,
    };
    let want = p.sample_rate;
    let best = cfgs
        .iter()
        .filter(|c| c.min_sample_rate().0 <= want && want <= c.max_sample_rate().0)
        .max_by_key(|c| (c.channels(), -fmt_rank(c.sample_format())))
        .map(|c| (*c).with_sample_rate(cpal::SampleRate(want)));
    let supported = match best {
        Some(s) => s,
        None => device.default_input_config().map_err(|e| e.to_string())?,
    };
    let config: cpal::StreamConfig = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(rate as usize * channels * 8);
    let failed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I8 => build::<i8>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::U8 => build::<u8>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::F64 => build::<f64>(&device, &config, prod, failed.clone(), dropped.clone()),
        f => return Err(format!("Unsupported sample format {f:?}")),
    }
    .map_err(|e| format!("Cannot open '{name}': {e}"))?;
    stream.play().map_err(|e| format!("Cannot start '{name}': {e}"))?;
    Ok(Input { guard: Box::new(stream), cons, failed, dropped, channels, rate, device: name })
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut prod: rtrb::Producer<f32>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| push_input(&mut prod, data, &dropped),
        move |err| {
            log::error!("audio stream error: {err}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
}
