//! TEMPORARY: `Bot::no_responce` (shared crate) hard-codes
//! `http://localhost:9821`. Inside a container that is the container itself,
//! so when `LOCALHOST_SHIM_TARGET` is set (e.g. `app:8081`) we listen on
//! 127.0.0.1:9821 and pipe connections there. Delete this module once the
//! shared crate takes its base URL from an env var.

pub async fn start(target: String) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:9821").await?;
    tracing::info!(target, "localhost:9821 shim for the shared crate's Bot socket");
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                continue;
            };
            let target = target.clone();
            tokio::spawn(async move {
                match tokio::net::TcpStream::connect(&target).await {
                    Ok(mut out) => {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                    }
                    Err(e) => tracing::warn!(target, "shim connect failed: {e}"),
                }
            });
        }
    });
    Ok(())
}
