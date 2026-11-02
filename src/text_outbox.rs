//! Per-peer TEXT backlog: one current metrics snapshot and bounded ordered replies.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::sync::{mpsc, watch};

const REPLY_CAPACITY: usize = 64;

#[derive(Clone)]
pub(super) struct Sender {
    replies: mpsc::Sender<Vec<u8>>,
    metrics: watch::Sender<Option<Vec<u8>>>,
    overflow_reported: Arc<AtomicBool>,
}
pub(super) struct Receiver {
    replies: mpsc::Receiver<Vec<u8>>,
    metrics: watch::Receiver<Option<Vec<u8>>>,
    metrics_open: bool,
    replies_open: bool,
}
pub(super) fn channel() -> (Sender, Receiver) {
    let (replies, rx) = mpsc::channel(REPLY_CAPACITY);
    let (metrics, latest) = watch::channel(None);
    (Sender { replies, metrics, overflow_reported: Arc::new(AtomicBool::new(false)) },
        Receiver { replies: rx, metrics: latest, metrics_open: true, replies_open: true })
}
impl Sender {
    pub(super) fn send(&self, message: Vec<u8>) -> Result<(), mpsc::error::SendError<Vec<u8>>> {
        self.replies.try_send(message).map_err(|error| {
            // Never wait for TEXT inside CONTROL's request handler: some viewers never open it.
            if matches!(error, mpsc::error::TrySendError::Full(_))
                && !self.overflow_reported.swap(true, Ordering::Relaxed) {
                log::warn!("TEXT reply queue full; dropping excess replies");
            }
            mpsc::error::SendError(error.into_inner())
        })
    }
    pub(super) fn metrics(&self, message: Vec<u8>) {
        let _ = self.metrics.send(Some(message));
    }
}
impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            if !self.metrics_open && !self.replies_open { return None; }
            tokio::select! {
                changed = self.metrics.changed(), if self.metrics_open => {
                    if changed.is_err() { self.metrics_open = false; continue; }
                    if let Some(message) = self.metrics.borrow_and_update().clone() { return Some(message); }
                }
                message = self.replies.recv(), if self.replies_open => match message {
                    Some(message) => return Some(message),
                    None => self.replies_open = false,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unopened_text_keeps_latest_metrics_and_bounded_fifo_replies() {
        let (tx, mut rx) = channel();
        for i in 0..10_000u32 { tx.metrics(i.to_le_bytes().to_vec()); }
        for i in 0..REPLY_CAPACITY { tx.send(vec![i as u8]).unwrap(); }
        assert_eq!(tx.send(vec![255]).unwrap_err().0, vec![255]);
        let mut replies = Vec::new();
        let mut metrics = Vec::new();
        for _ in 0..REPLY_CAPACITY + 1 {
            let message = rx.recv().await.unwrap();
            if message.len() == 4 { metrics.push(message); } else { replies.push(message[0]); }
        }
        assert_eq!(metrics, vec![9_999u32.to_le_bytes().to_vec()]);
        assert_eq!(replies, (0..REPLY_CAPACITY as u8).collect::<Vec<_>>());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await.is_err());
        tx.send(vec![100]).unwrap();
        assert_eq!(rx.recv().await, Some(vec![100]));
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn closed_text_rejects_replies() {
        let (tx, rx) = channel();
        tx.metrics(vec![13]);
        tx.send(vec![22]).unwrap();
        drop(rx);
        assert!(tx.send(vec![23]).is_err());
        tx.metrics(vec![14]);
    }
}
