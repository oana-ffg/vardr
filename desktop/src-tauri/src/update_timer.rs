use std::{
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::Duration,
};

/// Dropping the sender wakes this wait immediately when the app exits.
pub fn run(shutdown: Receiver<()>, mut check: impl FnMut() -> bool) {
    while check() {
        match shutdown.recv_timeout(Duration::from_secs(60)) {
            Err(RecvTimeoutError::Timeout) => (),
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn disconnect_wakes_the_minute_wait_without_another_check() {
        let (stop, shutdown) = mpsc::channel();
        let (checked, checks) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            run(shutdown, || {
                checked.send(()).unwrap();
                true
            });
            finished.send(()).unwrap();
        });
        checks.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(stop);
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(checks.try_iter().count(), 0);
    }
}
