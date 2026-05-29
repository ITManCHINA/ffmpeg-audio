use crate::sys;

/// A trait binding Rust native numeric types to FFmpeg's `AVSampleFormat`.
///
/// This trait is used to ensure type safety when extracting resampled audio data.
pub trait AudioSample: Copy + Send + Sync + 'static {
    const FORMAT: sys::AVSampleFormat;
}

impl AudioSample for f32 {
    const FORMAT: sys::AVSampleFormat = sys::AVSampleFormat_AV_SAMPLE_FMT_FLT;
}

impl AudioSample for i16 {
    const FORMAT: sys::AVSampleFormat = sys::AVSampleFormat_AV_SAMPLE_FMT_S16;
}

impl AudioSample for i32 {
    #[expect(clippy::use_self)]
    const FORMAT: sys::AVSampleFormat = sys::AVSampleFormat_AV_SAMPLE_FMT_S32;
}

impl AudioSample for u8 {
    const FORMAT: sys::AVSampleFormat = sys::AVSampleFormat_AV_SAMPLE_FMT_U8;
}
