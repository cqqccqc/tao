//! 公共 HTTP/SSE 层:所有 provider codec 共用。
//! 负责:重试(429/5xx/网络)、流空闲超时、取消传播、tracing span。
//! 见 docs/design/providers.md §4。

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::{Client, RequestBuilder, StatusCode, header};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::model::ModelError;

#[derive(Debug, Clone)]
pub struct HttpSseClient {
    client: Client,
    /// 单次请求(连接 + 收到首字节)最大重试次数。
    pub request_max_retries: u32,
    /// 流中断后整体重试次数(M1 暂未使用:流断直接上抛)。
    pub stream_max_retries: u32,
    pub stream_idle_timeout: Duration,
}

impl Default for HttpSseClient {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client build"),
            request_max_retries: 4,
            stream_max_retries: 3,
            stream_idle_timeout: Duration::from_secs(60),
        }
    }
}

impl HttpSseClient {
    /// 测试用:自定义重试与空闲超时。
    pub fn for_test(request_max_retries: u32, stream_idle_timeout: Duration) -> Self {
        Self {
            request_max_retries,
            stream_idle_timeout,
            ..Default::default()
        }
    }
}

/// SSE 事件:provider codec 解析 `event`/`data` 两字段。
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl HttpSseClient {
    /// 发起 SSE 请求,带重试与取消。
    /// `build_request` 在每次重试时调用,确保 header/body 重新生成。
    pub async fn sse_stream<F>(
        &self,
        build_request: F,
        cancel: &CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<SseEvent, ModelError>>, ModelError>
    where
        F: Fn(&Client) -> RequestBuilder + Send + Sync + 'static,
    {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            if cancel.is_cancelled() {
                return Err(ModelError::Retryable("已取消".into()));
            }
            let req = build_request(&self.client);
            match self.do_request(req, cancel).await {
                Ok(stream) => return Ok(stream.boxed()),
                Err(err) => {
                    let retryable = matches!(err, ModelError::Retryable(_));
                    if !retryable || attempt > self.request_max_retries {
                        warn!(attempt, error = %err, "SSE 请求最终失败");
                        return Err(err);
                    }
                    let backoff = backoff(attempt);
                    debug!(attempt, backoff_ms = backoff.as_millis(), "SSE 重试");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => return Err(ModelError::Retryable("已取消".into())),
                    }
                }
            }
        }
    }

    async fn do_request(
        &self,
        req: RequestBuilder,
        cancel: &CancellationToken,
    ) -> Result<impl Stream<Item = Result<SseEvent, ModelError>> + Send + 'static, ModelError> {
        let resp = tokio::select! {
            r = req.send() => r.map_err(map_reqwest_err)?,
            _ = cancel.cancelled() => return Err(ModelError::Retryable("已取消".into())),
        };
        let status = resp.status();
        if !status.is_success() {
            return Err(map_status_error(status, resp).await);
        }
        let sse = resp.bytes_stream().eventsource();
        Ok(idle_timeout(
            sse,
            self.stream_idle_timeout,
            cancel.child_token(),
        ))
    }
}

fn backoff(attempt: u32) -> Duration {
    let secs = 0.5_f64 * 2_f64.powi((attempt - 1) as i32);
    Duration::from_secs_f64(secs.min(30.0))
}

fn map_reqwest_err(e: reqwest::Error) -> ModelError {
    if e.is_timeout() || e.is_connect() {
        ModelError::Retryable(format!("网络错误: {e}"))
    } else if e.is_decode() {
        ModelError::Stream(format!("解码错误: {e}"))
    } else {
        ModelError::Fatal(format!("reqwest: {e}"))
    }
}

async fn map_status_error(status: StatusCode, resp: reqwest::Response) -> ModelError {
    let body = resp.text().await.unwrap_or_default();
    match status.as_u16() {
        401 | 403 => ModelError::Auth(format!("{}: {}", status, truncate(&body, 200))),
        429 => ModelError::Retryable(format!("429 限流: {}", truncate(&body, 200))),
        400 | 422 if body.contains("context_length") || body.contains("too long") => {
            ModelError::ContextLength(format!("上下文超长: {}", truncate(&body, 200)))
        }
        413 => ModelError::ContextLength(format!("请求体过大: {}", truncate(&body, 200))),
        s if (500..600).contains(&s) => {
            ModelError::Retryable(format!("{s}: {}", truncate(&body, 200)))
        }
        s => ModelError::Fatal(format!("{s}: {}", truncate(&body, 200))),
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n { s } else { &s[..n] }
}

/// 给 SSE 流套一个 idle timeout:超时产出 StreamError。
fn idle_timeout(
    s: impl Stream<
        Item = Result<
            eventsource_stream::Event,
            eventsource_stream::EventStreamError<reqwest::Error>,
        >,
    > + Send
    + 'static,
    dur: Duration,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<SseEvent, ModelError>> + Send + 'static {
    futures::stream::unfold(
        (s.boxed(), dur, cancel, tokio::time::Instant::now()),
        |(mut s, dur, cancel, last)| async move {
            if cancel.is_cancelled() {
                return None;
            }
            tokio::select! {
                item = s.next() => match item {
                    Some(Ok(ev)) => {
                        let event = if ev.event.is_empty() { None } else { Some(ev.event) };
                        let out = Ok(SseEvent { event, data: ev.data });
                        Some((out, (s, dur, cancel, tokio::time::Instant::now())))
                    }
                    Some(Err(e)) => {
                        let err = ModelError::Stream(format!("SSE 解析错误: {e}"));
                        Some((Err(err), (s, dur, cancel, tokio::time::Instant::now())))
                    }
                    None => None,
                },
                _ = tokio::time::sleep_until(last + dur) => {
                    let err = ModelError::Stream(format!("流空闲超过 {}ms", dur.as_millis()));
                    Some((Err(err), (s, dur, cancel, last + dur * 2)))
                }
            }
        },
    )
}

// re-export header helpers for codecs
pub use header::{HeaderMap, HeaderName, HeaderValue};
