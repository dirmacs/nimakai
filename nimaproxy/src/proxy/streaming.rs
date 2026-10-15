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
use std::time::Duration;

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
                    yield Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream stalled mid-stream",
                    ));
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
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        )),
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
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "upstream stalled mid-stream",
                ));
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
        assert!(
            matches!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut),
            "the failure must be a timeout, which is what the caller maps to a 504"
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
