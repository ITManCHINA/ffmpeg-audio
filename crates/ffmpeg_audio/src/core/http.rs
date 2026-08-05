use std::{
    io::{
        self,
        Error,
        ErrorKind,
        Read,
        Seek,
        SeekFrom,
    },
    pin::Pin,
    sync::{
        Arc,
        Mutex,
    },
    time::Duration,
};

use futures_util::TryStreamExt;
use reqwest::{
    Client,
    StatusCode,
    header,
};
use tokio::{
    io::{
        AsyncRead,
        AsyncReadExt,
    },
    runtime::{
        Builder,
        Runtime,
    },
};
use tokio_util::{
    io::StreamReader,
    sync::CancellationToken,
};

use crate::{
    Result,
    error::HttpError,
};

/// Soft Seek threshold
///
/// When the span of a forward seek is less than or equal to this value, addressing
/// is performed by directly reading and discarding data, avoiding breaking the
/// current TCP/TLS connection.
const SHORT_SEEK_THRESHOLD: u64 = 64 * 1024;

const MAX_RETRIES: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum allowed retry wait time to prevent thread deadlocks caused by absurdly
/// long wait times specified in the Retry-After header
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

const USER_AGENT: &str = "Lavf/62.12.101";

type AsyncReader = Pin<Box<dyn AsyncRead + Send + Sync + 'static>>;

enum FetchAction {
    Success(AsyncReader, u64),
    Retry(Option<Duration>),
    Fatal(HttpError),
}

struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

/// An opaque, thread-safe handle used to trigger and reset cancellation of network operations on
/// [`HttpAudioSource`].
#[derive(Clone, Default)]
pub struct HttpCancelHandle {
    inner: Arc<Mutex<CancellationToken>>,
}

impl HttpCancelHandle {
    /// Creates a new [`HttpCancelHandle`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CancellationToken::new())),
        }
    }

    /// Triggers immediate cancellation of ongoing network operations associated with this handle.
    pub fn cancel(&self) {
        self.inner.lock().unwrap().cancel();
    }

    /// Resets the cancellation state, allowing subsequent operations to proceed normally.
    ///
    /// This is typically called prior to a seek operation on a reused audio source.
    pub fn reset(&self) {
        let mut token = self.inner.lock().unwrap();
        if token.is_cancelled() {
            *token = CancellationToken::new();
        }
    }

    /// Returns `true` if the cancellation signal has been triggered and not yet reset.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.lock().unwrap().is_cancelled()
    }

    fn token(&self) -> CancellationToken {
        self.inner.lock().unwrap().clone()
    }
}

impl From<&Self> for HttpCancelHandle {
    fn from(handle: &Self) -> Self {
        handle.clone()
    }
}

/// An HTTP audio source that supports remote range requests and is compatible with `std::io::Read +
/// Seek`.
pub struct HttpAudioSource {
    /// The URL of the target audio stream.
    url: String,

    /// An internally reused HTTP client instance, managing the connection pool and timeout
    /// configuration.
    client: Client,

    /// The total size (in bytes) of the audio file, determined via server probing.
    content_length: u64,

    /// Tracks the current absolute byte offset within the file.
    current_pos: u64,

    /// The currently active HTTP body reader.
    /// This is set to `None` upon a large-span seek or an unrecoverable network error.
    body_reader: Option<AsyncReader>,

    /// The exclusive end offset of the currently active HTTP response range.
    stream_end: u64,

    /// Opaque cancellation handle passed in from the upper-layer application
    cancel_handle: HttpCancelHandle,

    /// Locally cached active token to eliminate Mutex lock overhead during high-frequency `read()`
    /// calls.
    active_token: CancellationToken,

    /// An isolated single-threaded Tokio runtime dedicated to driving current asynchronous network
    /// operations.
    rt: Runtime,
}

impl HttpAudioSource {
    /// Attempts to connect to and probe the target remote URL using a default cancellation token.
    ///
    /// Connection is permitted only if the server explicitly supports HTTP Range requests
    /// and provides a determinate content length.
    ///
    /// This is a convenience constructor that delegates to [`Self::new_with_cancel_handle`] with a
    /// newly created, non-canceled [`HttpCancelHandle`].
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the target remote audio stream.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The remote server does not support HTTP Range requests (fails to return HTTP 206).
    /// * The total stream length cannot be determined from the headers.
    /// * A transport or connection error occurs during the initial network probe.
    pub fn new(url: &str) -> Result<Self> {
        Self::new_with_cancel_handle(url, HttpCancelHandle::new())
    }

    /// Attempts to connect to and probe the target remote URL, allowing the initial connection
    /// and response probing phase to be interrupted.
    ///
    /// Connection is permitted only if the server explicitly supports HTTP Range requests
    /// and provides a determinate content length.
    ///
    /// The initial HTTP probe request runs inside an internally managed, single-threaded Tokio
    /// runtime. By passing an opaque [`HttpCancelHandle`], the caller can immediately abort this
    /// synchronous initialization block if the remote server hangs or is unresponsive during
    /// TCP connection, TLS handshake, or while waiting for response headers.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the target remote audio stream.
    /// * `handle` - An opaque cancellation handle ([`HttpCancelHandle`]) or reference to it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The operation is canceled via the provided `handle` (returns [`HttpError::Cancelled`]).
    /// * The remote server does not support HTTP Range requests (fails to return HTTP 206).
    /// * The total stream length cannot be determined.
    /// * A transport or connection error occurs during the initial network probe.
    pub fn new_with_cancel_handle(url: &str, handle: impl Into<HttpCancelHandle>) -> Result<Self> {
        let handle = handle.into();
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        let active_token = handle.token();
        let (content_length, stream_end, body_reader) = rt.block_on(async {
            tokio::select! {
                res = Self::probe_stream(&client, url) => res,

                () = active_token.cancelled() => {
                    Err(HttpError::Cancelled.into())
                }
            }
        })?;

        Ok(Self {
            url: url.to_string(),
            client,
            content_length,
            current_pos: 0,
            body_reader: Some(body_reader),
            stream_end,
            cancel_handle: handle,
            active_token,
            rt,
        })
    }

    fn refresh_active_token(&mut self) -> CancellationToken {
        self.active_token = self.cancel_handle.token();
        self.active_token.clone()
    }

    fn cancel_token(&mut self) -> CancellationToken {
        if self.active_token.is_cancelled() {
            self.refresh_active_token();
        }
        self.active_token.clone()
    }

    async fn probe_stream(client: &Client, url: &str) -> Result<(u64, u64, AsyncReader)> {
        let response = client
            .get(url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::RANGE, "bytes=0-")
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(HttpError::UnsupportedRange.into());
        }

        let content_range = Self::parse_content_range(&response)
            .ok_or_else(|| HttpError::InvalidContentRange("missing or malformed header".into()))?;
        if content_range.start != 0 {
            return Err(HttpError::InvalidContentRange(format!(
                "range starts at {}, expected 0",
                content_range.start
            ))
            .into());
        }
        if content_range.end >= content_range.total {
            return Err(HttpError::InvalidContentRange(format!(
                "range ends at {}, total length is {}",
                content_range.end, content_range.total
            ))
            .into());
        }
        let content_length = content_range.total;

        #[cfg(feature = "tracing")]
        tracing::info!("Probing successful. Stream length: {content_length} bytes.");

        let stream = response.bytes_stream().map_err(io::Error::other);
        let reader: AsyncReader = Box::pin(StreamReader::new(stream));

        Ok((content_length, content_range.end + 1, reader))
    }

    fn parse_content_range(response: &reqwest::Response) -> Option<ContentRange> {
        let value = response
            .headers()
            .get(header::CONTENT_RANGE)?
            .to_str()
            .ok()?;
        let (unit, range_and_total) = value.trim().split_once(' ')?;
        if unit != "bytes" {
            return None;
        }
        let (range, total) = range_and_total.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse().ok()?;
        let end = end.parse().ok()?;
        let total = total.parse().ok()?;
        (start <= end).then_some(ContentRange { start, end, total })
    }

    async fn execute_seek_request(&self, target_pos: u64) -> FetchAction {
        let range_header = format!("bytes={target_pos}-");
        let result = self
            .client
            .get(&self.url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::RANGE, range_header)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();

                match status {
                    StatusCode::PARTIAL_CONTENT => {
                        let Some(content_range) = Self::parse_content_range(&resp) else {
                            return FetchAction::Fatal(HttpError::InvalidContentRange(
                                "missing or malformed header".into(),
                            ));
                        };
                        if content_range.start != target_pos {
                            return FetchAction::Fatal(HttpError::InvalidContentRange(format!(
                                "range starts at {}, expected {target_pos}",
                                content_range.start
                            )));
                        }
                        if content_range.total != self.content_length {
                            return FetchAction::Fatal(HttpError::InvalidContentRange(format!(
                                "total length is {}, expected {}",
                                content_range.total, self.content_length
                            )));
                        }

                        let stream = resp.bytes_stream().map_err(io::Error::other);
                        let reader: AsyncReader = Box::pin(StreamReader::new(stream));

                        FetchAction::Success(reader, content_range.end + 1)
                    }
                    _ if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                        let delay = resp
                            .headers()
                            .get(header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.trim().parse::<u64>().ok())
                            .map(Duration::from_secs);

                        #[cfg(feature = "tracing")]
                        if let Some(d) = delay {
                            tracing::info!("Server requested Retry-After: {}s", d.as_secs());
                        } else {
                            tracing::warn!(
                                "Server error HTTP {}. Tagged for retry.",
                                status.as_u16()
                            );
                        }

                        FetchAction::Retry(delay)
                    }
                    StatusCode::OK if target_pos != 0 => {
                        FetchAction::Fatal(HttpError::UnsupportedRange)
                    }
                    _ if status.is_client_error() => {
                        #[cfg(feature = "tracing")]
                        tracing::error!("Fatal client error: HTTP {}", status.as_u16());

                        FetchAction::Fatal(HttpError::Status(status.as_u16()))
                    }
                    _ => FetchAction::Fatal(HttpError::Status(status.as_u16())),
                }
            }

            #[allow(unused_variables)]
            Err(e) => {
                #[cfg(feature = "tracing")]
                tracing::warn!("Transport error during seek request: {e}. Tagged for retry.");

                FetchAction::Retry(None)
            }
        }
    }

    async fn hard_seek_with_retry(&mut self, target_pos: u64) -> Result<()> {
        self.body_reader = None;

        if target_pos >= self.content_length {
            self.current_pos = target_pos;
            return Ok(());
        }

        let cancel_token = self.cancel_token();
        let mut retry_policy = RetryPolicy::new(cancel_token.clone());

        loop {
            if cancel_token.is_cancelled() {
                return Err(HttpError::Cancelled.into());
            }

            match self.execute_seek_request(target_pos).await {
                FetchAction::Success(reader, stream_end) => {
                    self.body_reader = Some(reader);
                    self.stream_end = stream_end;
                    self.current_pos = target_pos;
                    return Ok(());
                }
                FetchAction::Fatal(err) => {
                    return Err(err.into());
                }
                FetchAction::Retry(custom_delay) => {
                    retry_policy.wait_next(custom_delay).await?;
                }
            }
        }
    }
}

struct RetryPolicy {
    attempt: u32,
    max_retries: u32,
    max_retry_delay: Duration,
    cancel_token: CancellationToken,
}

impl RetryPolicy {
    const fn new(cancel_token: CancellationToken) -> Self {
        Self {
            attempt: 0,
            max_retries: MAX_RETRIES,
            max_retry_delay: MAX_RETRY_DELAY,
            cancel_token,
        }
    }

    async fn wait_next(&mut self, custom_delay: Option<Duration>) -> Result<()> {
        self.attempt += 1;
        if self.attempt > self.max_retries {
            return Err(HttpError::Timeout.into());
        }

        let wait_time =
            custom_delay.unwrap_or_else(|| Duration::from_secs(1 << (self.attempt - 1)));

        if wait_time > self.max_retry_delay {
            #[cfg(feature = "tracing")]
            tracing::error!("Retry delay {wait_time:?} exceeds limit. Aborting.");
            return Err(HttpError::Timeout.into());
        }

        #[cfg(feature = "tracing")]
        tracing::warn!(
            "Waiting {wait_time:?} before next attempt (Attempt {}/{})...",
            self.attempt,
            self.max_retries
        );

        tokio::select! {
            () = tokio::time::sleep(wait_time) => Ok(()),
            () = self.cancel_token.cancelled() => Err(HttpError::Cancelled.into()),
        }
    }
}

impl Read for HttpAudioSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.current_pos >= self.content_length || buf.is_empty() {
            return Ok(0);
        }

        let mut network_retried = false;

        loop {
            let cancel_token = self.cancel_token();
            let rt_handle = self.rt.handle().clone();

            if self.body_reader.is_none() {
                let target_pos = self.current_pos;
                let token = cancel_token.clone();

                rt_handle.block_on(async {
                    tokio::select! {
                        res = self.hard_seek_with_retry(target_pos) => res.map_err(io::Error::from),
                        () = token.cancelled() => Err(Error::new(ErrorKind::Interrupted, "Cancelled")),
                    }
                })?;

                if self.body_reader.is_none() {
                    return Ok(0);
                }
            }

            let reader = self.body_reader.as_mut().unwrap();
            let remaining = self.stream_end.saturating_sub(self.current_pos);
            let read_buf_len = std::cmp::min(remaining, buf.len() as u64) as usize;

            let read_result = rt_handle.block_on(async {
                tokio::select! {
                    res = tokio::time::timeout(RECV_TIMEOUT, reader.read(&mut buf[..read_buf_len])) => {
                        res.unwrap_or_else(|_| Err(Error::new(ErrorKind::TimedOut, "Stream read timeout")))
                    }
                    () = cancel_token.cancelled() => {
                        Err(Error::new(ErrorKind::Interrupted, "Cancelled"))
                    }
                }
            });

            match read_result {
                Ok(0) => {
                    if self.current_pos < self.stream_end {
                        return Err(Error::other(format!(
                            "Stream ends prematurely at {}, should be {}",
                            self.current_pos, self.stream_end
                        )));
                    }

                    if self.current_pos < self.content_length {
                        self.body_reader = None;
                    } else {
                        return Ok(0);
                    }
                }
                Ok(n) => {
                    self.current_pos += n as u64;
                    return Ok(n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    return Err(e);
                }
                Err(e) => {
                    if network_retried {
                        return Err(e);
                    }

                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        "Network read error at offset {}: {}. Attempting to recover...",
                        self.current_pos,
                        e
                    );

                    self.body_reader = None;
                    network_retried = true;
                }
            }
        }
    }
}

impl Seek for HttpAudioSource {
    /// Seeks to an offset in bytes.
    ///
    /// Note on Seek Strategy:
    ///
    /// Implementing a lazy seek (deferring HTTP Range requests until the next `read()`)
    /// might improve performance.
    ///
    /// However, to strictly match FFmpeg's internal protocol behavior (where `http_seek`
    /// immediately attempts reconnection and returns exact I/O results to the demuxer), we
    /// perform a synchronous, immediate seek here.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                let new_pos = self.current_pos.cast_signed() + offset;
                if new_pos < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Cannot seek to a negative position",
                    ));
                }
                new_pos.cast_unsigned()
            }
            SeekFrom::End(offset) => {
                let new_pos = self.content_length.cast_signed() + offset;
                if new_pos < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Cannot seek to a negative position",
                    ));
                }
                new_pos.cast_unsigned()
            }
        };

        if target_pos == self.current_pos {
            return Ok(target_pos);
        }

        if target_pos > self.current_pos {
            let delta = target_pos - self.current_pos;

            if delta <= SHORT_SEEK_THRESHOLD && self.body_reader.is_some() {
                #[cfg(feature = "tracing")]
                tracing::debug!(
                    "Soft-seeking to offset {target_pos} by draining {delta} remaining byte(s)",
                );

                let mut discard_buf = [0u8; 4096];
                let mut remaining = delta;
                let mut soft_seek_success = true;

                while remaining > 0 {
                    let to_read = std::cmp::min(remaining, discard_buf.len() as u64) as usize;
                    match self.read(&mut discard_buf[..to_read]) {
                        Ok(0) => {
                            soft_seek_success = false;
                            break;
                        }
                        Ok(n) => {
                            remaining -= n as u64;
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => {
                            return Err(e);
                        }
                        Err(_) => {
                            soft_seek_success = false;
                            break;
                        }
                    }
                }

                if soft_seek_success {
                    return Ok(target_pos);
                }

                #[cfg(feature = "tracing")]
                tracing::warn!("Soft seek failed. Falling back to hard seek.");
            }
        }

        let rt_handle = self.rt.handle().clone();
        let cancel_token = self.cancel_token();

        rt_handle.block_on(async {
            tokio::select! {
                res = self.hard_seek_with_retry(target_pos) => res.map_err(io::Error::from),
                () = cancel_token.cancelled() => Err(Error::new(ErrorKind::Interrupted, "Cancelled")),
            }
        })?;

        Ok(target_pos)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{
            Read,
            Seek,
            SeekFrom,
        },
        thread,
        time::Instant,
    };

    use super::*;
    use crate::{
        AudioError,
        error::HttpError::Cancelled,
    };

    const TEST_URL: &str = "http://localhost:8000/test.mp3";
    const CHAOS_URL: &str = "http://localhost:8000/chaos.mp3";
    const BLOCKHOLE_URL: &str = "http://192.0.2.1/test.mp3";

    #[test]
    #[ignore = "A network connection is required. Use tests/server.go to set up the mock server."]
    fn test_basic_read_and_seek() {
        let mut source = HttpAudioSource::new(TEST_URL).unwrap();

        let total_length = source.content_length;
        assert!(total_length > 1024 * 1024, "File too small for testing");

        let mut buf1 = vec![0u8; 100];
        let n = source.read(&mut buf1).unwrap();
        assert_eq!(n, 100);
        assert_eq!(source.current_pos, 100);

        for (i, &byte) in buf1.iter().enumerate() {
            assert_eq!(
                byte,
                (i % 256) as u8,
                "Data mismatch at initial read offset {i}"
            );
        }

        let new_pos = source.seek(SeekFrom::Current(1000)).unwrap();
        assert_eq!(new_pos, 1100);
        assert_eq!(source.current_pos, 1100);

        let mut buf2 = vec![0u8; 10];
        let n = source.read(&mut buf2).unwrap();
        assert_eq!(n, 10);

        for (i, &byte) in buf2.iter().enumerate() {
            assert_eq!(
                byte,
                ((1100 + i) % 256) as u8,
                "Data mismatch after soft seek at offset {}",
                1100 + i
            );
        }

        let rewind_pos = source.seek(SeekFrom::Start(50)).unwrap();
        assert_eq!(rewind_pos, 50);

        let mut buf3 = vec![0u8; 10];
        let n = source.read(&mut buf3).unwrap();
        assert_eq!(n, 10);

        for (i, &byte) in buf3.iter().enumerate() {
            assert_eq!(
                byte,
                ((50 + i) % 256) as u8,
                "Data mismatch after hard seek at offset {}",
                50 + i
            );
        }

        source.seek(SeekFrom::End(0)).unwrap();
        let mut eof_buf = vec![0u8; 10];
        let eof_n = source.read(&mut eof_buf).unwrap();
        assert_eq!(eof_n, 0, "Should return 0 at EOF");
    }

    #[test]
    fn test_instant_cancellation() {
        let cancel_handle = HttpCancelHandle::new();
        let cancel_handle_clone = cancel_handle.clone();

        let handle = thread::spawn(move || {
            HttpAudioSource::new_with_cancel_handle(BLOCKHOLE_URL, cancel_handle_clone)
        });

        thread::sleep(Duration::from_millis(100));

        let cancel_start_time = Instant::now();

        cancel_handle.cancel();

        let result = handle.join().unwrap();

        let elapsed = cancel_start_time.elapsed();

        assert!(
            elapsed < Duration::from_millis(50),
            "Cancellation took too long! Elapsed: {elapsed:?}"
        );

        match result {
            Err(e) => {
                assert!(
                    matches!(e, AudioError::Http(Cancelled)),
                    "Expected cancellation error, got: {e:?}"
                );
            }
            Ok(_) => panic!("Should not have successfully connected to a black hole!"),
        }
    }

    #[test]
    #[ignore = "A network connection is required. Use tests/server.go to set up the mock server."]
    fn test_retry_on_503() {
        let start_time = Instant::now();

        let mut source = HttpAudioSource::new(CHAOS_URL).unwrap();

        let seek_pos = source.seek(SeekFrom::Start(100_000)).unwrap();
        assert_eq!(seek_pos, 100_000);

        let mut buf = vec![0u8; 10];
        let n = source.read(&mut buf).unwrap();
        assert_eq!(n, 10);

        for (i, &byte) in buf.iter().enumerate() {
            assert_eq!(byte, ((100_000 + i) % 256) as u8);
        }

        let elapsed = start_time.elapsed();

        assert!(
            elapsed >= Duration::from_secs(2),
            "Should have retried twice and waited at least 2 seconds, elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn test_read_zero_buffer() {
        let cancel_handle = HttpCancelHandle::new();
        let mut source = HttpAudioSource {
            url: "http://localhost/dummy.mp3".to_string(),
            client: Client::new(),
            content_length: 1024,
            current_pos: 0,
            body_reader: None,
            stream_end: 0,
            cancel_handle: cancel_handle.clone(),
            active_token: cancel_handle.token(),
            rt: tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        };

        let mut empty_buf = [0u8; 0];
        let res = source.read(&mut empty_buf);

        assert!(res.is_ok(), "read(0) must return Ok(0)");
        assert_eq!(res.unwrap(), 0);
        assert_eq!(source.current_pos, 0, "read(0) must not modify current_pos");
        assert!(
            source.body_reader.is_none(),
            "read(0) must not initialize or reset body_reader"
        );
    }

    #[test]
    fn test_shared_token_cancellation_and_reset() {
        let cancel_handle = HttpCancelHandle::new();

        assert!(!cancel_handle.is_cancelled());

        cancel_handle.cancel();
        assert!(cancel_handle.is_cancelled());

        cancel_handle.reset();
        assert!(
            !cancel_handle.is_cancelled(),
            "Reset token should restore uncancelled state"
        );
    }
}
