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
    let mut reader = AudioReader::new(file, i32::try_from(sample_rate)?, i32::from(channels))?;

    let info = reader.source_info();
    println!(
        "📄 源文件信息: {} ({} Hz, {} 声道)",
        info.codec_name.as_deref().unwrap_or("unknown"),
        info.sample_rate,
        info.channels
    );

    // 针对 IFF/DFF 容器进行全量数据包扫描获取精确时长
    if info.codec_name.as_deref().is_some_and(|name| name.starts_with("dsd")) {
        println!("🔍 正在对 IFF/DFF 容器进行全量数据包扫描...");
        if let Some(exact_duration) = reader.scan_exact_duration() {
            println!("⏱️ 扫描完毕！精准时长为: {:.2} 秒", exact_duration.as_secs_f64());
        } else {
            println!("⚠️ 无法扫描获取该文件的精准时长");
        }
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

    while let Some(frame) = reader.receive_frame()? {
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
