use std::ffi::CStr;

use thiserror::Error;

use crate::sys;

// https://github.com/FFmpeg/FFmpeg/blob/239f2c733de417201d7ad3b3b8b0d9b63285b2b1/libavutil/error.h#L86
const AV_ERROR_MAX_STRING_SIZE: usize = 64;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("End of file")]
    Eof,

    #[error("Resource temporarily unavailable")]
    Eagain,

    #[error("FFmpeg error {0}: {1}")]
    FFmpeg(i32, String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Audio format mismatch: requested type does not match resampler output format")]
    FormatMismatch,

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
}

impl AudioError {
    #[must_use]
    pub fn from_ffmpeg(code: i32) -> Self {
        match code {
            sys::AVERROR_EOF => Self::Eof,
            sys::AVERROR_EAGAIN => Self::Eagain,
            _ => {
                let mut buf = [0u8; AV_ERROR_MAX_STRING_SIZE];

                unsafe {
                    sys::av_strerror(code, buf.as_mut_ptr().cast::<libc::c_char>(), buf.len());
                }

                let error_message = CStr::from_bytes_until_nul(&buf).map_or_else(
                    |_| "Unknown FFmpeg error when parsing C string".to_string(),
                    |c_str| c_str.to_string_lossy().into_owned(),
                );

                Self::FFmpeg(code, error_message)
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, AudioError>;

/// 一个方便的宏，用于在调用 FFmpeg C 函数后快速捕获错误
///
/// 如果返回值大于等于 0，则直接返回该值；
/// 如果小于 0，则自动将其转换为 [`AudioError`] 并用 `?` 抛出
#[macro_export]
macro_rules! fferr {
    ($expr:expr) => {{
        let ret = $expr;
        if ret < 0 {
            return Err($crate::error::AudioError::from_ffmpeg(ret));
        }
        ret
    }};
}
