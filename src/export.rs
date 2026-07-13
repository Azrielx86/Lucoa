use std::path::PathBuf;
use std::sync::Arc;

use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::app::AppEvent;
use crate::tab::TabId;

/// Batches of matched line texts to append to the export file.
pub type ExportBatch = Vec<Arc<str>>;

/// Spawn the per-tab export writer. The task exits (after a final flush) when
/// every sender clone is dropped — toggle-off, tab close, and quit all reduce
/// to that. I/O errors are reported through `event_tx`; the returned handle
/// yields the number of lines written and is awaited at quit to guarantee the
/// final flush happened.
pub fn spawn_export(
    path: PathBuf,
    tab: TabId,
    event_tx: mpsc::Sender<AppEvent>,
) -> (mpsc::Sender<ExportBatch>, JoinHandle<u64>) {
    let (tx, rx) = mpsc::channel(1024);
    let handle = tokio::spawn(export_task(path, tab, event_tx, rx));
    (tx, handle)
}

async fn export_task(
    path: PathBuf,
    tab: TabId,
    event_tx: mpsc::Sender<AppEvent>,
    mut rx: mpsc::Receiver<ExportBatch>,
) -> u64 {
    let mut written = 0u64;
    let result: std::io::Result<()> = async {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut w = BufWriter::new(file);
        let mut dirty = false;
        let mut flush_tick = interval(Duration::from_millis(250));
        flush_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(lines) => {
                        for line in lines {
                            w.write_all(line.as_bytes()).await?;
                            w.write_all(b"\n").await?;
                            written += 1;
                        }
                        dirty = true;
                    }
                    None => break, // all senders gone: flush and exit
                },
                _ = flush_tick.tick(), if dirty => {
                    w.flush().await?;
                    dirty = false;
                }
            }
        }
        w.flush().await?;
        w.shutdown().await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let _ = event_tx
            .send(AppEvent::FileError {
                tab,
                msg: format!("export {}: {e}", path.display()),
            })
            .await;
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flushes_everything_on_sender_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        let (event_tx, _event_rx) = mpsc::channel(8);

        let (tx, handle) = spawn_export(path.clone(), 1, event_tx);
        tx.send(vec![Arc::from("one"), Arc::from("two")])
            .await
            .unwrap();
        tx.send(vec![Arc::from("three")]).await.unwrap();
        drop(tx); // toggle-off / tab close / quit all reduce to this

        let written = handle.await.unwrap();
        assert_eq!(written, 3);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn reports_open_failure_via_event_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("out.log");
        let (event_tx, mut event_rx) = mpsc::channel(8);

        let (tx, handle) = spawn_export(path, 7, event_tx);
        drop(tx);
        assert_eq!(handle.await.unwrap(), 0);
        match event_rx.recv().await.unwrap() {
            AppEvent::FileError { tab, .. } => assert_eq!(tab, 7),
            _ => panic!("expected FileError"),
        }
    }
}
