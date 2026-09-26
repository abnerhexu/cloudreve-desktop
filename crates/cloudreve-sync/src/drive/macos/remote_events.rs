use crate::drive::{commands::MountCommand, mounts::Mount, sync::SyncMode};
use cloudreve_api::api::explorer::FileEventsApi;
use std::{sync::Arc, time::Duration};

impl Mount {
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let config = self.get_config().await;
        super::placeholder::validate_path(&config.sync_path, &config.sync_path)?;
        anyhow::ensure!(
            config.sync_path.is_dir(),
            "Choose an existing local sync folder"
        );
        if config.enabled {
            self.start_fs_watcher().await?;
        }
        Ok(())
    }

    pub async fn process_remote_events(mount: Arc<Self>) {
        let config = mount.get_config().await;
        if !config.enabled {
            return;
        }
        let schedule = || {
            let _ = mount.command_tx.send(MountCommand::Sync {
                local_paths: vec![config.sync_path.clone()],
                mode: SyncMode::FullHierarchy,
                user_initiated: false,
            });
        };
        // Keep the fallback timer outside the SSE connection future. Connecting
        // (including a proxy accepting TCP but never returning headers) must not
        // suspend periodic reconciliation.
        let events = async {
            loop {
                match mount
                    .cr_client
                    .subscribe_file_events(&config.remote_path)
                    .await
                {
                    Ok(mut subscription) => {
                        mount.set_event_push_subscribed(true).await;
                        loop {
                            {
                                let event = subscription.next_event().await;
                                use cloudreve_api::models::explorer::FileEvent;
                                match event {
                                    Ok(Some(
                                        FileEvent::Event(_)
                                        | FileEvent::Subscribed
                                        | FileEvent::Resumed,
                                    )) => schedule(),
                                    Ok(Some(FileEvent::KeepAlive)) => {}
                                    _ => break,
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Remote events unavailable; using periodic reconciliation")
                    }
                }
                mount.set_event_push_subscribed(false).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        };
        reconcile_while_listening(events, schedule, Duration::from_secs(30)).await;
    }
}

async fn reconcile_while_listening(
    events: impl std::future::Future<Output = ()>,
    schedule: impl Fn(),
    period: Duration,
) {
    tokio::pin!(events);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => schedule(),
            _ = &mut events => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stalled_event_connection_does_not_block_periodic_scans() {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let done = tokio::sync::Notify::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = reconcile_while_listening(std::future::pending(), || {
                    if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2 {
                        done.notify_one();
                    }
                }, Duration::from_millis(10)) => panic!("listener stopped"),
                _ = done.notified() => {},
            }
        })
        .await
        .expect("stalled SSE blocked the scan timer");
    }
}
