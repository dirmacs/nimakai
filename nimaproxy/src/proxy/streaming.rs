//! Forwarding an upstream response without waiting for all of it.
//!
//! The whole-body pattern this replaces — `try_collect` inside a single timeout
//! — silently converts a streaming response into a batch one. Measured on the
//! live gateway, same request and same params, two arms:
//!
//! ```text
//!     streaming direct: TTFB 0.33 s, 262 chunks, 7.9 s total
//!     through the proxy: 504 after 15 s, then the retry also 504
//! ```
//!
//! The upstream needed 80 s+ to produce as a single body what it streamed in
//! 7.9 s, so a 15 s whole-body deadline was killing responses that were
//! progressing normally. Every lane on the host runs its model through this
//! proxy, and long generations never completed.

use axum::body::Body;
use bytes::Bytes;
use futures::StreamExt;
use std::error::Error;
use std::fmt;
use std::time::Duration;

/// The payload carried by an idle-guard stall error.
///
/// A stall and an upstream I/O failure are different events and callers must
/// be able to tell them apart without pattern-matching on a message string:
/// a stall is a *silent* upstream and is retryable (nothing has reached the
/// client yet), while an I/O error usually means the connection broke and is
/// not. Conveying that only through [`std::io::ErrorKind::TimedOut`] left every
/// caller guessing — and at least one caller read a stall as a 502 instead of
/// a 504.
///
/// This type is attached as the `io::Error` payload, so the signal is
/// discoverable by downcast (see [`is_stall`]) rather than by sniffing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStalled {
    /// The inter-chunk window that elapsed with no bytes.
    pub window: Duration,
}

impl fmt::Display for StreamStalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "upstream stalled mid-stream")
    }
}

impl Error for StreamStalled {}

/// Build the `io::Error` the idle guard yields when the window elapses.
///
/// The kind stays [`std::io::ErrorKind::TimedOut`] so axum still aborts the
/// response, and existing consumers that key on the kind keep working; the
/// typed payload is what makes the stall *identifiable*.
pub fn stalled_error(window: Duration) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, StreamStalled { window })
}

/// Report whether `e` is an idle-guard stall rather than an upstream failure.
///
/// Prefer this over matching `ErrorKind::TimedOut`: the kind is necessary but
/// not sufficient, because an upstream may legitimately time out on its own.
/// Only a [`StreamStalled`] payload proves the *guard* fired.
pub fn is_stall(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|payload| payload.is::<StreamStalled>())
}

/// Wrap a response body stream so a gap longer than `window` between chunks
/// fails the stream instead of hanging the connection.
///
/// Bounding the *inter-chunk* gap preserves the protection the whole-body
/// timeout provided — a stalled upstream still fails, within about one window
/// of stalling — while letting a long but healthy generation finish.
///
/// On timeout the stream yields an `io::Error`, which axum turns into an
/// aborted response. That is the honest outcome: bytes have already reached
/// the client, so there is no status code left to correct it, and faking a
/// clean end would tell the caller the generation completed.
pub fn with_idle_timeout<S>(
    stream: S,
    window: Duration,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    async_stream::stream! {
        futures::pin_mut!(stream);
        loop {
            match tokio::time::timeout(window, stream.next()).await {
                // A chunk arrived within the window: forward it.
                Ok(Some(Ok(chunk))) => yield Ok(chunk),
                // The upstream itself errored: forward that, then stop.
                Ok(Some(Err(e))) => {
                    yield Err(e);
                    break;
                }
                // The upstream closed cleanly after its last chunk.
                Ok(None) => break,
                // No chunk within the window: the upstream stalled mid-stream.
                Err(_) => {
                    yield Err(stalled_error(window));
                    break;
                }
            }
        }
    }
}

/// Prepends a chunk already consumed by the caller to the rest of the stream.
///
/// The first byte is read separately so its arrival can be bounded (that is the
/// only bound that can still produce a 504, because nothing has been sent to
/// the client yet). This puts it back at the head so no bytes are lost.
pub fn prepend(
    first: Bytes,
    rest: impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    futures::stream::once(async move { Ok(first) }).chain(rest)
}

/// Convert a `reqwest` byte stream into the `io::Error` stream both helpers
/// above expect.
pub fn map_upstream<S>(stream: S) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures::Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    stream.map(|chunk| match chunk {
        Ok(bytes) => Ok(bytes),
        Err(e) => Err(std::io::Error::other(e.to_string())),
    })
}

/// Assemble a complete body under an inter-chunk idle bound.
///
/// Returns `None` if the upstream went quiet for longer than `window` between
/// chunks. This is what lets a mid-stream stall stay retryable for non-streaming
/// responses: nothing has been written to the caller, so the caller can still be
/// given a 504 and the request can be retried on another key.
///
/// A body that merely takes a long time to finish is *not* a stall here, which
/// is the whole point — the old code judged total duration.
pub async fn assemble_with_idle_bound(
    stream: impl futures::Stream<Item = Result<Bytes, std::io::Error>>,
    window: Duration,
) -> Result<Vec<u8>, std::io::Error> {
    let mut collected = Vec::new();
    futures::pin_mut!(stream);
    loop {
        match tokio::time::timeout(window, stream.next()).await {
            Ok(Some(Ok(chunk))) => collected.push(chunk),
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => return Ok(collected.concat()),
            Err(_) => {
                return Err(stalled_error(window));
            }
        }
    }
}

/// A streaming body that applies [`with_idle_timeout`] to `stream`.
pub fn idle_guarded_body<S>(stream: S, window: Duration) -> Body
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    Body::from_stream(with_idle_timeout(stream, window))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        gap: Duration,
        count: usize,
    ) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
        futures::stream::unfold(0usize, move |i| async move {
            if i >= count {
                return None;
            }
            tokio::time::sleep(gap).await;
            Some((Ok(Bytes::from(format!("chunk{i}"))), i + 1))
        })
    }

    /// A stream whose total duration exceeds the window but whose gaps do not
    /// must be assembled in full. This is the fleet's scenario: the whole-body
    /// deadline this replaced killed exactly these responses.
    #[tokio::test]
    async fn test_long_but_healthy_body_is_not_a_stall() {
        // 6 chunks x 40 ms = 240 ms of generation against a 100 ms window.
        let body = assemble_with_idle_bound(
            chunks(Duration::from_millis(40), 6),
            Duration::from_millis(100),
        )
        .await
        .expect("a healthy slow body must not be reported as a stall");
        let text = String::from_utf8(body).unwrap();
        for i in 0..6 {
            assert!(text.contains(&format!("chunk{i}")), "lost {i}: {text}");
        }
    }

    /// A stream that stops mid-body must fail, so a dead upstream cannot hold a
    /// connection open. Without this the test above could pass by simply never
    /// failing.
    #[tokio::test]
    async fn test_stalled_body_is_an_error() {
        // One chunk, then silence far beyond the window.
        let stalled = futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(Bytes::from("first"))
        })
        .chain(futures::stream::pending::<Result<Bytes, std::io::Error>>());

        let result = assemble_with_idle_bound(stalled, Duration::from_millis(80)).await;
        assert!(
            result.is_err(),
            "a stalled upstream must be an error, not a hang"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind(), std::io::ErrorKind::TimedOut),
            "the kind must stay TimedOut so axum still aborts the response"
        );
        assert!(
            is_stall(&err),
            "and it must carry the typed payload, which is what distinguishes a silent upstream from a broken connection"
        );
    }

    /// The payload must survive the guard's `io::Error` round trip, because
    /// that is the only way a caller can tell a stall from an I/O error without
    /// sniffing the message string.
    #[test]
    fn test_stall_payload_is_discoverable_and_exclusive() {
        let stall = stalled_error(Duration::from_millis(250));
        assert!(
            is_stall(&stall),
            "a guard stall must be identifiable as a stall"
        );
        assert_eq!(
            stall
                .get_ref()
                .and_then(|p| p.downcast_ref::<StreamStalled>())
                .map(|s| s.window),
            Some(Duration::from_millis(250)),
            "the payload must carry the window that elapsed"
        );

        // The negative arm: an ordinary upstream I/O error is NOT a stall. If
        // this arm were missing, the test above would pass by always matching.
        let upstream_broke = std::io::Error::other("connection reset by peer");
        assert!(
            !is_stall(&upstream_broke),
            "an upstream I/O error must not be classified as a stall"
        );
        let upstream_timed_out = std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "upstream timed out on its own",
        );
        assert!(
            !is_stall(&upstream_timed_out),
            "an upstream-sourced timeout shares the kind but is not a guard stall; only the payload proves it"
        );
    }

    /// `prepend` must not drop or reorder the head chunk.
    #[tokio::test]
    async fn test_prepend_keeps_the_head_chunk_first() {
        let rest = chunks(Duration::from_millis(1), 2);
        let stream = prepend(Bytes::from("head"), rest);
        let collected: Vec<Bytes> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(
            collected,
            vec![
                Bytes::from("head"),
                Bytes::from("chunk0"),
                Bytes::from("chunk1")
            ]
        );
    }
}
