use std::{
    marker::PhantomData,
    ptr::NonNull,
    time::Duration,
};

use crate::sys;

/// A safe, zero-copy wrapper around FFmpeg's raw `AVFrame`.
///
/// This wrapper is useful for 1-to-N zero-copy dispatching to multiple downstream
/// `Resampler` instances simultaneously.
pub struct AudioFrame<'a> {
    ptr: NonNull<sys::AVFrame>,
    time_base: sys::AVRational,
    _marker: PhantomData<&'a mut ()>,
}

impl AudioFrame<'_> {
    /// Creates a new `AudioFrame` wrapper.
    ///
    /// ## Safety
    /// This method is for internal crate use. The caller ensures that the provided
    /// `ptr` is a valid FFmpeg `AVFrame` and that its memory remains valid for the
    /// duration of the lifetime.
    pub(crate) const fn new(ptr: *const sys::AVFrame, time_base: sys::AVRational) -> Self {
        Self {
            ptr: NonNull::new(ptr.cast_mut()).expect("FFmpeg returned a null AVFrame pointer"),
            time_base,
            _marker: PhantomData,
        }
    }

    /// Extracts the underlying raw FFmpeg `AVFrame` pointer.
    ///
    /// This is used internally to pass the raw frame data into FFmpeg's FFI functions
    /// (such as the resampling context).
    pub(crate) const fn as_ptr(&self) -> *const sys::AVFrame {
        self.ptr.as_ptr()
    }

    /// Returns the number of audio samples (per channel) contained in this frame.
    ///
    /// For example, if a stereo frame contains 1024 samples, this will return `1024`
    /// (not 2048).
    #[must_use]
    pub const fn samples(&self) -> usize {
        unsafe { (*self.ptr.as_ptr()).nb_samples as usize }
    }

    /// Returns the Presentation Timestamp (PTS) of this frame, if available.
    ///
    /// ## Returns
    /// - `Some(Duration)` representing the exact playback time of the frame.
    /// - `None` if the underlying frame lacks a valid PTS (`AV_NOPTS_VALUE`).
    #[must_use]
    pub fn pts(&self) -> Option<Duration> {
        unsafe {
            let pts = (*self.ptr.as_ptr()).pts;
            if pts == sys::AV_NOPTS_VALUE {
                None
            } else {
                let bq = sys::AVRational {
                    num: 1,
                    den: sys::AV_TIME_BASE.cast_signed(),
                };
                let us = sys::av_rescale_q(pts, self.time_base, bq);
                Some(Duration::from_micros(us.max(0).cast_unsigned()))
            }
        }
    }
}
