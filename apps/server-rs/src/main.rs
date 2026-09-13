use opensessions_runtime::shared::resolve_server_settings;
use opensessions_server::{ServerConfig, default_state_source_from_env, start_server};
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = resolve_server_settings(|key| std::env::var(key).ok());
    let mut config = ServerConfig::new(settings.host, settings.port, settings.pid_file);
    if let Some(source) = default_state_source_from_env(|key| std::env::var(key).ok()) {
        config = config.with_state_source(source);
    }
    let server = start_server(config).await?;
    // The tmux plugin restarts the server with a plain `kill` on update, so
    // treat SIGTERM/SIGINT like `/quit`: tell every sidebar to exit and remove
    // our tmux hooks instead of vanishing and leaving stale clients behind.
    let trigger = server.shutdown_trigger();
    tokio::spawn(async move {
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(_) => return,
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        let _ = trigger.send(());
    });
    server.wait_shutdown().await?;
    Ok(())
}
