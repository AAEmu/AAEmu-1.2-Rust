use super::SocketReader;
use tokio::sync::oneshot;

struct OnDrop(Option<oneshot::Sender<()>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(());
    }
}

async fn reader() -> (SocketReader, oneshot::Receiver<()>) {
    let (started, ready) = oneshot::channel();
    let (stopped, done) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _resource = OnDrop(Some(stopped));
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    (SocketReader(Some(task)), done)
}

#[tokio::test]
async fn explicit_close_and_cancelled_owner_release_reader_resources() {
    let (mut task, stopped) = reader().await;
    task.close().await;
    assert!(stopped.await.is_ok());
    task.close().await;

    let (task, stopped) = reader().await;
    drop(task);
    assert!(stopped.await.is_ok());
}
