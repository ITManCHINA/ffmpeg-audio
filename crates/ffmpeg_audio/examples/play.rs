use std::{
    env,
    fs::File,
    thread,
    time::Duration,
};

use cpal::traits::{
    DeviceTrait as _,
    HostTrait as _,
    StreamTrait as _,
};
use ffmpeg_audio::{
    AudioReader,
    ResampleOptions,
    log::init_ffmpeg_logging,
};
use ringbuf::{
    HeapRb,
    traits::{
        Consumer as _,
        Observer as _,
        Producer as _,
        Split as _,
    },
};
use tracing_subscriber::{
    EnvFilter,
    fmt,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
        )
        .init();

    init_ffmpeg_logging();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: {} <音频文件路径>", args[0]);
        std::process::exit(1);
    }
    let file_path = &args[1];

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("未找到默认音频输出设备");

    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate();
    let channels = config.channels();

    println!("🎵 声卡已就绪: {sample_rate} Hz, {channels} 声道");

    let file = File::open(file_path)?;
    let reader = AudioReader::new(file)?;

    let quick_duration = reader.duration();

    let options = ResampleOptions::new()
        .sample_rate(i32::try_from(sample_rate)?)
        .channels(i32::from(channels))
        .format::<f32>();

    let mut resampled = reader.into_resampled(options)?;

    let info = resampled.source_info();
    println!(
        "📄 源文件信息: {} ({} Hz, {} 声道)",
        info.codec_name.as_deref().unwrap_or("unknown"),
        info.sample_rate,
        info.channels
    );

    let duration_info = if let Some(dur) = quick_duration {
        Some(dur)
    } else if let Some(dur) = resampled.scan_exact_duration(true)? {
        Some(dur)
    } else {
        resampled.scan_exact_duration(false)?
    };

    if let Some(d) = duration_info {
        let total_secs = d.as_secs();
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        let millis = d.subsec_millis();
        println!("⏱️ 音频时长: {minutes:02}:{seconds:02}.{millis:03}");
    } else {
        println!("⚠️ 无法获取该文件的时长");
    }

    let buffer_capacity = (sample_rate * u32::from(channels) * 4) as usize;
    let rb = HeapRb::<f32>::new(buffer_capacity);
    let (mut producer, mut consumer) = rb.split();

    let err_fn = |err| eprintln!("声卡输出流发生错误: {err}");
    let cpal_config = config.config();

    let stream = device.build_output_stream(
        &cpal_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for sample in data.iter_mut() {
                *sample = consumer.try_pop().unwrap_or(0.0);
            }
        },
        err_fn,
        None,
    )?;

    stream.play()?;
    println!("▶️ 开始播放...");

    while let Some(frame) = resampled.receive_frame_as::<f32>()? {
        let mut written = 0;

        while written < frame.len() {
            let pushed = producer.push_slice(&frame[written..]);
            written += pushed;

            if pushed == 0 {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    while !producer.is_empty() {
        thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}
