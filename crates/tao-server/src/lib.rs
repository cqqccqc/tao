//! # tao-server
//!
//! Op/Event 的 wire 传输:`tao proto`(stdio JSONL,单客户端)与
//! `tao serve`(TCP 多客户端,broadcast 扇出)。stderr 留日志,stdout/连接只走协议。

mod session;

use anyhow::Result;
use tao_protocol::wire;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use session::{WireSessionHandle, spawn};

/// `tao proto` 入口:stdio JSONL wire(单客户端)。
pub async fn run_proto() -> Result<()> {
    let config = tao_core::config::Config::load(&tao_core::config::LoadOpts::default())?;
    let (handle, sid) = spawn(config).await?;
    tracing::info!("tao proto: session {sid}");
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let writer = tokio::io::stdout();
    run_connection(handle, reader, writer).await;
    Ok(())
}

/// `tao serve` 入口:TCP 多客户端,所有连接 attach 同一 session,事件 broadcast 扇出。
/// 任一连接可 submit Op(含 ApprovalResponse);所有连接都收到事件流。
pub async fn run_serve(port: u16) -> Result<()> {
    let config = tao_core::config::Config::load(&tao_core::config::LoadOpts::default())?;
    let (handle, sid) = spawn(config).await?;
    tracing::info!("tao serve: listen 0.0.0.0:{port}, session {sid}");

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        let (stream, addr) = listener.accept().await?;
        let handle = handle.clone();
        tracing::info!("serve: client connected {addr}");
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let reader = BufReader::new(read_half);
            run_connection(handle, reader, write_half).await;
        });
    }
}

/// 单个 wire 连接的读写循环:writer task 把 broadcast Event 编码写出,
/// 主循环读 Submission Op 提交给 actor。
async fn run_connection<R, W>(handle: WireSessionHandle, mut reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut rx = handle.subscribe();
    let mut writer = writer;
    let write_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let line = match wire::encode_event(&ev) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let line = line + "\n";
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        match wire::decode_submission(line) {
            Ok(sub) => handle.submit(sub).await,
            Err(e) => tracing::warn!("proto: 解析 Submission 失败: {e}"),
        }
    }
    write_task.abort();
}
