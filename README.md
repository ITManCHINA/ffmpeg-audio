# ffmpeg-audio

A lightweight FFmpeg audio decoding wrapper designed for music player applications.

## Features

* Simple and intuitive API
* Pure Rust compilation via [cc](https://github.com/rust-lang/cc-rs) — no C build system required
* Fully statically linked with zero external FFmpeg dependencies
* Bundled with a specific, up-to-date FFmpeg version (currently 8.1.1)
* Optional `tracing` feature for FFmpeg log integration

## Supported Audio Formats

This crate leverages FFmpeg's decoding capabilities and supports almost all audio formats, including:

* MP3, AAC, FLAC, WAV, OGG, Opus, WMA, ALAC, AIFF, and more

You can find the complete list in [generate_config.ts](./scripts/generate_config.ts)

## Supported Sample Formats

The resampler output supports the following Rust-native sample types:

| Type  | FFmpeg Format  |
| ----- | -------------- |
| `f32` | 32-bit Float   |
| `i16` | 16-bit Signed  |
| `i32` | 32-bit Signed  |
| `u8`  | 8-bit Unsigned |

## Usage

```toml
[dependencies]
ffmpeg_audio = { git = "https://github.com/apoint123/ffmpeg-audio" }

```

```rust
use std::fs::File;
use ffmpeg_audio::{AudioReader, ResampleOptions};

// 1. Initialize the decoding engine
let reader = AudioReader::new(File::open("song.mp3")?)?;

// 2. Configure target audio parameters (e.g., 48kHz, Stereo, 32-bit Float)
let options = ResampleOptions::new()
    .sample_rate(48000)
    .channels(2)
    .format::<f32>();

// 3. Transform into a resampled data pipeline
let mut resampled = reader.into_resampled(options)?;

// 4. Safely pull typed audio frames
while let Some(samples) = resampled.receive_frame_as::<f32>()? {
    // `samples` is strictly typed as &[f32]
    // process your interleaved samples here
}

```

## Supported Platforms

* `x86_64-pc-windows-msvc`
* `i686-pc-windows-msvc`
* `aarch64-pc-windows-msvc`
* `aarch64-linux-android`
* `armv7-linux-androideabi`
* `i686-linux-android`
* `x86_64-linux-android`
* `aarch64-apple-darwin`
* `x86_64-apple-darwin`
* `x86_64-unknown-linux-gnu`
* `aarch64-unknown-linux-gnu`
* `aarch64-apple-ios`

Other target platforms are not currently supported.

## Examples

The `crates/ffmpeg_audio/examples` directory contains runnable examples:

* **`play.rs`** — A complete audio player using `cpal` for audio output
* **`metadata.rs`** — Extracts audio metadata and cover art from files

Run an example with:

```sh
cargo run --example play -- path/to/audio.mp3
cargo run --example metadata -- path/to/audio.flac
```

## Project Structure

This is a Cargo workspace containing two crates:

* **`ffmpeg_audio`** — High-level Rust API for audio decoding and resampling
* **`ffmpeg_audio_sys`** — Low-level FFI bindings to FFmpeg's C libraries

## Limitations

This crate focuses solely on audio decoding. The following features are not included:

* Video processing
* Encoders, muxers, and filters
* Swscale (video scaling/conversion)
* Hardware acceleration
* Device and network protocol support

## License

[GPL 3.0](LICENSE)