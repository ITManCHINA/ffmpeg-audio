use std::{
    io::Cursor,
    time::Duration,
};

use ffmpeg_audio::{
    AudioReader,
    ResampleOptions,
};

fn generate_sine_wav(duration_secs: f32) -> Vec<u8> {
    let sample_rate: u32 = 44100;
    let freq: f32 = 440.0;
    let num_samples = (sample_rate as f32 * duration_secs) as u32;

    let mut data = Vec::with_capacity(44 + (num_samples * 2) as usize);

    data.extend_from_slice(b"RIFF");
    let file_size = 36 + num_samples * 2;
    data.extend_from_slice(&file_size.to_le_bytes());
    data.extend_from_slice(b"WAVE");

    data.extend_from_slice(b"fmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * 2;
    data.extend_from_slice(&byte_rate.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());

    data.extend_from_slice(b"data");
    let data_size = num_samples * 2;
    data.extend_from_slice(&data_size.to_le_bytes());

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let sample = (f32::sin(2.0 * std::f32::consts::PI * freq * t) * 16000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }

    data
}

#[test]
fn test_audio_pipeline_and_signal_validation() {
    let wav_data = generate_sine_wav(1.0);
    let source = Cursor::new(wav_data);

    let target_sample_rate = 48000;
    let target_channels = 2;

    let reader = AudioReader::new(source).expect("初始化 AudioReader 失败");

    let options = ResampleOptions::new()
        .sample_rate(target_sample_rate)
        .channels(target_channels)
        .format::<f32>();

    let mut resampled = reader
        .into_resampled(options)
        .expect("初始化 ResampledReader 失败");

    let mut total_samples = 0;
    let mut energy_sum: f64 = 0.0;

    while let Some(frame) = resampled
        .receive_frame_as::<f32>()
        .expect("解码过程中发生错误")
    {
        assert_eq!(
            frame.len() % target_channels as usize,
            0,
            "输出缓冲区长度未与声道数对齐"
        );

        for &sample in frame {
            energy_sum = f64::from(sample).mul_add(f64::from(sample), energy_sum);
        }
        total_samples += frame.len();
    }

    assert!(
        (95900..=96100).contains(&total_samples),
        "样本数量异常! 预期约 96000，实际为 {total_samples}"
    );

    let rms = f64::sqrt(energy_sum / total_samples as f64);
    assert!(
        rms > 0.1,
        "静音错误：重采样器输出的几乎全是 0.0，波形能量过低 (RMS: {rms})"
    );
}

#[test]
fn test_audio_duration() {
    let wav_data = generate_sine_wav(2.0);
    let source = Cursor::new(wav_data);

    let reader = AudioReader::new(source).unwrap();
    let duration = reader.duration().expect("应能拿到 WAV 时长");
    let secs = duration.as_secs_f64();
    assert!((1.99..=2.01).contains(&secs), "时长应约为 2s，实际 {secs}");
}

#[test]
fn test_audio_seek_functionality() {
    let wav_data = generate_sine_wav(2.0);
    let source = Cursor::new(wav_data);

    let reader = AudioReader::new(source).unwrap();
    let mut resampled = reader
        .into_resampled(
            ResampleOptions::new()
                .sample_rate(48000)
                .channels(2)
                .format::<f32>(),
        )
        .unwrap();

    let _ = resampled.receive_frame_as::<f32>().unwrap();
    let _ = resampled.receive_frame_as::<f32>().unwrap();

    let target = Duration::from_secs_f32(1.0);
    resampled.seek(target).expect("Seek 调用失败");

    let frame_after_seek = resampled
        .receive_frame_as::<f32>()
        .expect("Seek 后读取帧报错")
        .expect("Seek 后立刻遇到了非预期的 EOF");

    assert!(!frame_after_seek.is_empty(), "Seek 后读取到了空数据包");
}

#[test]
fn test_current_playback_time_initially_none() {
    let wav_data = generate_sine_wav(1.0);
    let reader = AudioReader::new(Cursor::new(wav_data)).unwrap();

    assert!(
        reader.current_playback_time().is_none(),
        "解码开始前，current_playback_time 应为 None"
    );
}

#[test]
fn test_current_playback_time_advances() {
    let wav_data = generate_sine_wav(1.0);
    let reader = AudioReader::new(Cursor::new(wav_data)).unwrap();
    let mut resampled = reader
        .into_resampled(
            ResampleOptions::new()
                .sample_rate(48000)
                .channels(2)
                .format::<f32>(),
        )
        .unwrap();

    resampled.receive_frame_as::<f32>().unwrap();
    let first_pts = resampled.current_playback_time();
    assert!(
        first_pts.is_some(),
        "解码至少一帧后，current_playback_time 应为 Some"
    );

    while resampled.receive_frame_as::<f32>().unwrap().is_some() {}
    let last_pts = resampled.current_playback_time();
    assert!(
        last_pts >= first_pts,
        "播放时间应随解码推进而增大，first={first_pts:?} last={last_pts:?}"
    );
}

#[test]
fn test_current_playback_time_resets_after_seek() {
    let wav_data = generate_sine_wav(2.0);
    let reader = AudioReader::new(Cursor::new(wav_data)).unwrap();
    let mut resampled = reader
        .into_resampled(
            ResampleOptions::new()
                .sample_rate(48000)
                .channels(2)
                .format::<f32>(),
        )
        .unwrap();

    resampled.receive_frame_as::<f32>().unwrap();
    assert!(resampled.current_playback_time().is_some());

    resampled.seek(Duration::from_secs(0)).unwrap();
    assert!(
        resampled.current_playback_time().is_none(),
        "Seek 后 current_playback_time 应重置为 None"
    );
}

#[test]
fn test_full_decode_no_panic() {
    let wav_data = generate_sine_wav(1.0);
    let reader = AudioReader::new(Cursor::new(wav_data)).unwrap();
    let mut resampled = reader
        .into_resampled(
            ResampleOptions::new()
                .sample_rate(48000)
                .channels(2)
                .format::<f32>(),
        )
        .unwrap();

    while resampled.receive_frame_as::<f32>().unwrap().is_some() {}
}
