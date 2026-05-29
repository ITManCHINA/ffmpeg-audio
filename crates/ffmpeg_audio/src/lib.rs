pub mod error;
pub mod io;
pub mod log;

mod decoder;
mod demuxer;
mod format;
mod frame;
mod resampler;

use std::{
    collections::HashMap,
    ffi::CStr,
    io::{
        Read,
        Seek,
    },
    time::Duration,
};

pub use error::{
    AudioError,
    Result,
};
pub use ffmpeg_audio_sys as sys;
pub use format::AudioSample;
pub use frame::AudioFrame;
pub use resampler::{
    ResampleOptions,
    Resampler,
};

use crate::{
    decoder::Decoder,
    demuxer::Demuxer,
    log::init_ffmpeg_logging,
};

#[derive(Debug, Clone)]
pub struct AudioCover {
    pub data: Vec<u8>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceAudioInfo {
    /// Samples per second.
    pub sample_rate: i32,

    /// Number of channels.
    pub channels: i32,

    /// The average bitrate of the encoded data (in bits per second).
    pub bit_rate: i64,

    /// Audio sample format.
    pub sample_fmt: Option<String>,

    /// The name of a codec.
    pub codec_name: Option<String>,

    /// This is the number of valid bits in each output sample.
    ///
    /// If the sample format has more bits, the least significant bits are additional
    /// padding bits, which are always `0`. Use right shifts to reduce the sample
    /// to its actual size. For example, audio formats with 24 bit samples will
    /// have `bits_per_raw_sample` set to `24`, and format set to `AV_SAMPLE_FMT_S32`.
    ///
    /// To get the original sample use `(int32_t)sample >> 8`.
    ///
    /// For ADPCM this might be `12` or `16` or similar
    ///
    /// Can be 0
    pub bits_per_sample: i32,
}

pub struct AudioReader {
    decoder: Decoder,
    demuxer: Demuxer,
    is_exhausted: bool,

    time_base: sys::AVRational,
    current_pts: Option<Duration>,

    source_info: SourceAudioInfo,
}

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for AudioReader {}

impl AudioReader {
    pub fn new<T>(source: T) -> Result<Self>
    where
        T: Read + Seek + Send + 'static,
    {
        init_ffmpeg_logging();

        let io_ctx = io::IoContext::new(source)?;
        let demuxer = Demuxer::new(io_ctx)?;
        let codec_params = demuxer.stream_codec_params();
        let decoder = Decoder::new(codec_params)?;
        let time_base = demuxer.time_base()?;

        let source_info = unsafe {
            let codec_id = (*codec_params).codec_id;
            let codec_name_ptr = sys::avcodec_get_name(codec_id);
            let codec_name = if codec_name_ptr.is_null() {
                None
            } else {
                Some(
                    CStr::from_ptr(codec_name_ptr)
                        .to_string_lossy()
                        .into_owned(),
                )
            };

            let src_sample_fmt = decoder.sample_fmt();
            let fmt_name_ptr = sys::av_get_sample_fmt_name(src_sample_fmt);
            let sample_fmt_str = if fmt_name_ptr.is_null() {
                None
            } else {
                Some(CStr::from_ptr(fmt_name_ptr).to_string_lossy().into_owned())
            };

            let stream_bit_rate = (*codec_params).bit_rate;
            let bit_rate = if stream_bit_rate > 0 {
                stream_bit_rate
            } else {
                demuxer.bit_rate()
            };

            let bits_per_raw = (*codec_params).bits_per_raw_sample;
            let bits_per_coded = (*codec_params).bits_per_coded_sample;
            let bits_per_sample = if bits_per_raw > 0 {
                bits_per_raw
            } else {
                bits_per_coded
            };

            SourceAudioInfo {
                sample_rate: decoder.sample_rate(),
                channels: decoder.channels(),
                bit_rate,
                sample_fmt: sample_fmt_str,
                codec_name,
                bits_per_sample,
            }
        };

        Ok(Self {
            decoder,
            demuxer,
            is_exhausted: false,
            time_base,
            current_pts: None,
            source_info,
        })
    }

    /// Builds a [`Resampler`] pipeline tailored to this audio stream.
    ///
    /// This helper method automatically extracts the native channel layout, sample
    /// format, and sample rate from the underlying decoder, using them as the input
    /// configuration for the newly created resampler.
    ///
    /// This is an advanced API specifically designed for **1-to-N zero-copy dispatching**.
    /// It allows you to instantiate multiple distinct resamplers (e.g., one for audio
    /// playback, one for FFT spectrum analysis) and feed them the exact same safely
    /// borrowed [`AudioFrame`] without incurring any redundant decoding or memory
    /// allocation overhead.
    ///
    /// ## Arguments
    /// * `options` - The target `ResampleOptions` specifying the desired output
    ///   sample rate, channel count, and data format.
    ///
    /// ## Returns
    /// * `Ok(Resampler)` - A fully initialized resampler ready to process frames.
    ///
    /// ## Errors
    /// Returns an [`AudioError`] if the provided `options` are invalid, or if the
    /// internal FFmpeg `SwrContext` allocation and initialization fail.
    pub fn build_resampler(&self, options: ResampleOptions) -> Result<Resampler> {
        Resampler::new(
            &self.decoder.channel_layout(),
            self.decoder.sample_fmt(),
            self.decoder.sample_rate(),
            options,
        )
    }

    /// Reads and decodes the next available audio frame from the source stream.
    ///
    /// This method pulls a packet from the underlying demuxer, sends it to the decoder,
    /// retrieves the decoded raw frame, and finally returns a safe, zero-copy
    /// [`AudioFrame`] wrapper. You can pass its reference to multiple independent
    /// [`Resampler`] pipelines simultaneously without cloning the underlying audio data.
    ///
    /// ## Returns
    /// - `Ok(Some(AudioFrame))` if a frame was successfully decoded and is ready for use.
    /// - `Ok(None)` if the end of the audio stream (EOF) has been reached.
    /// - `Err(AudioError)` if an underlying I/O or FFmpeg decoding error occurs.
    pub fn receive_frame(&mut self) -> Result<Option<AudioFrame<'_>>> {
        if self.is_exhausted {
            return Ok(None);
        }

        loop {
            match self.decoder.receive_frame() {
                Ok(Some(frame)) => {
                    unsafe {
                        let pts = (*frame).pts;
                        if pts != sys::AV_NOPTS_VALUE {
                            let bq = sys::AVRational {
                                num: 1,
                                den: sys::AV_TIME_BASE.cast_signed(),
                            };
                            let us = sys::av_rescale_q(pts, self.time_base, bq);
                            self.current_pts =
                                Some(Duration::from_micros(us.max(0).cast_unsigned()));
                        }
                    }

                    return Ok(Some(AudioFrame::new(frame, self.time_base)));
                }
                Err(AudioError::Eagain) => match self.demuxer.read_packet()? {
                    Some(packet) => {
                        self.decoder.send_packet(packet)?;
                    }
                    None => {
                        self.decoder.send_eof_flush()?;
                    }
                },
                Ok(None) => {
                    self.is_exhausted = true;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Consumes the current [`AudioReader`] and wraps it in a [`ResampledReader`]
    /// using the provided resampling configuration.
    ///
    /// This establishes a pipeline from decoding to resampling, ready for data extraction.
    pub fn into_resampled(self, options: ResampleOptions) -> Result<ResampledReader> {
        let resampler = Resampler::new(
            &self.decoder.channel_layout(),
            self.decoder.sample_fmt(),
            self.decoder.sample_rate(),
            options,
        )?;

        Ok(ResampledReader {
            reader: self,
            resampler,
        })
    }

    #[must_use]
    pub const fn current_playback_time(&self) -> Option<Duration> {
        self.current_pts
    }

    #[must_use]
    pub const fn source_info(&self) -> &SourceAudioInfo {
        &self.source_info
    }

    #[must_use]
    pub fn metadata(&self) -> HashMap<String, String> {
        self.demuxer.metadata()
    }

    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.demuxer.duration()
    }

    /// 针对 DFF (IFF) 格式，通过快速扫描底层数据包获取绝对精准的时长。
    /// 本操作会读取全文件包，耗时与文件大小成正比，建议在后台线程执行并缓存结果。
    ///
    /// # 注意
    /// 本操作会重置播放进度至文件开头，并清空解码器与重采样器的内部缓冲区。
    pub fn scan_exact_duration(&mut self) -> Result<Option<Duration>> {
        let res = self.demuxer.scan_exact_duration();
        if !matches!(res, Ok(None)) {
            self.decoder.flush();
            self.is_exhausted = false;
            self.current_pts = None;
        }
        match res {
            Ok(Some(opt_duration)) => Ok(opt_duration),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[must_use]
    pub fn cover(&self) -> Option<AudioCover> {
        self.demuxer.cover()
    }

    pub fn seek(&mut self, target: Duration) -> Result<()> {
        self.demuxer.seek_to(target)?;
        self.decoder.flush();
        self.is_exhausted = false;
        self.current_pts = None;

        Ok(())
    }
}

/// A wrapper combining an [`AudioReader`] and a [`Resampler`].
///
/// This provides a streamlined pipeline that automatically handles
/// packet reading, decoding, and format conversion on the fly.
pub struct ResampledReader {
    reader: AudioReader,
    resampler: Resampler,
}

impl ResampledReader {
    /// Pulls the next frame of audio data, automatically decoded and
    /// resampled to the target configuration.
    ///
    /// ## Type Safety
    /// The generic type `T` MUST exactly match the format specified in
    /// `ResampleOptions` (e.g., `f32`, `i16`). Otherwise, an
    /// [`AudioError::FormatMismatch`] will be returned.
    pub fn receive_frame_as<T: AudioSample>(&mut self) -> Result<Option<&[T]>> {
        loop {
            let frame = self.reader.receive_frame()?;

            if let Some(frame) = frame {
                let has_data = self.resampler.process::<T>(Some(&frame))?;

                if has_data {
                    return Ok(Some(self.resampler.output_as::<T>()));
                }
            } else {
                let has_data = self.resampler.process::<T>(None)?;

                if has_data {
                    return Ok(Some(self.resampler.output_as::<T>()));
                }
                return Ok(None);
            }
        }
    }

    /// Returns the presentation timestamp (PTS) of the currently decoded frame.
    #[must_use]
    pub const fn current_playback_time(&self) -> Option<Duration> {
        self.reader.current_playback_time()
    }

    /// Returns the metadata and specifications of the original audio source.
    #[must_use]
    pub const fn source_info(&self) -> &SourceAudioInfo {
        self.reader.source_info()
    }

    /// Seeks the underlying audio stream to the specified target duration.
    ///
    /// Also flushes both the decoder and the resampler.
    pub fn seek(&mut self, target: Duration) -> Result<()> {
        self.reader.seek(target)?;
        self.resampler.flush()?;
        Ok(())
    }
}
