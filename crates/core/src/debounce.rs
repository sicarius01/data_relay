use crate::{
    bus::{FsEvent, ProcessRequest, Reason},
    config::Config,
    paths::normalize,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

struct Pending {
    task: JoinHandle<()>,
    first: Instant,
    generation: u64,
    path: PathBuf,
}

/// A timer only reports its generation. The owner alone removes entries and queues
/// work, so a timer expiring concurrently with a new event cannot erase that event.
pub async fn run(
    mut events: mpsc::UnboundedReceiver<FsEvent>,
    out: mpsc::Sender<ProcessRequest>,
    mut config: watch::Receiver<Arc<Config>>,
    cancel: CancellationToken,
) {
    let (fired_tx, mut fired_rx) = mpsc::unbounded_channel();
    let mut pending = HashMap::<String, Pending>::new();
    let mut generation = 0u64;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            changed = config.changed() => {
                if changed.is_err() {break;}
                let cfg = config.borrow().clone();
                pending.retain(|_, p| {
                    let retain = crate::watch::matches(&cfg, &p.path);
                    if !retain {p.task.abort();}
                    retain
                });
            }
            Some((key, token, reason)) = fired_rx.recv() => {
                if pending.get(&key).is_some_and(|p|p.generation==token) {
                    let p = pending.remove(&key).unwrap();
                    tokio::select! { _ = cancel.cancelled()=>break, _ = out.send(ProcessRequest::Process {path:p.path,reason})=>{} }
                }
            }
            event = events.recv() => match event {
                None => break,
                Some(FsEvent::Removed(path)) => {
                    if let Some(old) = pending.remove(&normalize(&path).to_string()) {old.task.abort();}
                    tokio::select! { _ = cancel.cancelled()=>break, _ = out.send(ProcessRequest::Removed(path))=>{} }
                }
                Some(FsEvent::Changed(path)) => {
                    let key = normalize(&path).to_string();
                    let now = Instant::now();
                    let first = pending.remove(&key).map(|old|{old.task.abort(); old.first}).unwrap_or(now);
                    let cfg = config.borrow().clone();
                    let quiet = now + Duration::from_secs(cfg.agent.debounce_sec.max(1));
                    let maximum = first + Duration::from_secs(cfg.agent.max_wait_sec.max(1));
                    if now>=maximum {
                        tokio::select! { _ = cancel.cancelled()=>break, _ = out.send(ProcessRequest::Process{path,reason:Reason::MaxWait})=>{} }
                        continue;
                    }
                    let deadline = quiet.min(maximum);
                    let reason = if maximum <= quiet {Reason::MaxWait} else {Reason::Quiet};
                    generation = generation.wrapping_add(1);
                    let token = generation;
                    let tx = fired_tx.clone(); let timer_key = key.clone();
                    let task = tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        let _ = tx.send((timer_key, token, reason));
                    });
                    pending.insert(key, Pending {task, first, generation: token, path});
                }
            },
        }
    }
    for (_, p) in pending {
        p.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn queued_event_flood_cannot_extend_an_expired_maximum() {
        let mut cfg = Config::default();
        cfg.agent.debounce_sec = 10;
        cfg.agent.max_wait_sec = 2;
        let (_cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg));
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, mut result) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(rx, out, cfg_rx, cancel.clone()));
        tx.send(FsEvent::Changed("a.csv".into())).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        for _ in 0..10000 {
            tx.send(FsEvent::Changed("a.csv".into())).unwrap();
        }
        let processed = tokio::time::timeout(Duration::from_millis(1), result.recv())
            .await
            .unwrap();
        assert!(matches!(
            processed,
            Some(ProcessRequest::Process {
                reason: Reason::MaxWait,
                ..
            })
        ));
        cancel.cancel();
        worker.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn continuous_events_force_max_wait_and_deletion_cancels() {
        let mut cfg = Config::default();
        cfg.agent.debounce_sec = 3;
        cfg.agent.max_wait_sec = 5;
        let (_cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg));
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, mut result) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(rx, out, cfg_rx, cancel.clone()));
        for _ in 0..5 {
            tx.send(FsEvent::Changed("a.csv".into())).unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        tokio::task::yield_now().await;
        assert!(matches!(
            result.recv().await,
            Some(ProcessRequest::Process {
                reason: Reason::MaxWait,
                ..
            })
        ));
        tx.send(FsEvent::Changed("b.csv".into())).unwrap();
        tx.send(FsEvent::Removed("b.csv".into())).unwrap();
        assert!(matches!(
            result.recv().await,
            Some(ProcessRequest::Removed(_))
        ));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(result.try_recv().is_err());
        cancel.cancel();
        worker.await.unwrap();
    }
}
