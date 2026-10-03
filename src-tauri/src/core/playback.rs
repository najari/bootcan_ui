//! Interruptible file playback. Filtered display does not alter player frames.
use super::{
    batcher::FrameBatcher,
    trace_player::{PlaybackState, TracePlayer},
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{sync::RwLock, time::Duration};

/// Returns true only when this generation naturally reaches the end.
pub async fn run(
    player: Arc<RwLock<TracePlayer>>,
    batcher: FrameBatcher,
    generation: Arc<AtomicU64>,
    ticket: u64,
) -> bool {
    let mut remaining = Duration::ZERO;
    loop {
        if generation.load(Ordering::Acquire) != ticket {
            return false;
        }
        let state = player.read().await.get_state();
        match state {
            PlaybackState::Stopped => return false,
            PlaybackState::Paused => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            PlaybackState::Playing => {}
        }
        if !remaining.is_zero() {
            let step = remaining.min(Duration::from_millis(10));
            tokio::time::sleep(step).await;
            remaining = remaining.saturating_sub(step);
            continue;
        }
        let mut guard = player.write().await;
        if guard.get_state() != PlaybackState::Playing {
            continue;
        }
        if generation.load(Ordering::Acquire) != ticket {
            return false;
        }
        match guard.get_next_frame() {
            Some((frame, delay)) => {
                batcher.push(frame);
                remaining = delay;
            }
            None => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> (Arc<RwLock<TracePlayer>>, FrameBatcher, Arc<AtomicU64>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("play.asc");
        std::fs::write(
            &path,
            "base hex timestamps absolute\n0.0 1 100 Rx d 1 01\n0.1 1 100 Rx d 1 02\n",
        )
        .unwrap();
        let mut player = TracePlayer::new();
        player.load_file(path, None, None).await.unwrap();
        player.start().unwrap();
        (
            Arc::new(RwLock::new(player)),
            FrameBatcher::new(),
            Arc::new(AtomicU64::new(1)),
        )
    }
    #[tokio::test]
    async fn pause_resume_keeps_the_task_and_interframe_delay() {
        let (p, b, g) = setup().await;
        let task = tokio::spawn(run(p.clone(), b.clone(), g, 1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        p.write().await.pause();
        assert_eq!(b.drain().len(), 1);
        tokio::time::sleep(Duration::from_millis(130)).await;
        assert!(b.drain().is_empty());
        p.write().await.resume();
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap());
        assert_eq!(b.drain()[0].data, [2]);
    }
    #[tokio::test]
    async fn replacing_generation_cancels_pending_delivery() {
        let (p, b, g) = setup().await;
        let task = tokio::spawn(run(p, b.clone(), g.clone(), 1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(b.drain().len(), 1);
        g.fetch_add(1, Ordering::AcqRel);
        assert!(!task.await.unwrap());
        assert!(b.drain().is_empty());
    }
}
