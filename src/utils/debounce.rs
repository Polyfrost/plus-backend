use std::{
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub(crate) struct Debouncer {
	pending: Arc<AtomicBool>,
	notify: Arc<Notify>,
}

impl Debouncer {
	pub(crate) fn spawn<F, Fut>(delay: Duration, action: F) -> Self
	where
		F: Fn() -> Fut + Send + 'static,
		Fut: Future<Output = ()> + Send,
	{
		let debouncer = Self {
			pending: Arc::new(AtomicBool::new(false)),
			notify: Arc::new(Notify::new()),
		};
		let worker = debouncer.clone();
		tokio::spawn(async move {
			loop {
				worker.notify.notified().await;
				tokio::time::sleep(delay).await;
				worker.pending.store(false, Ordering::SeqCst);
				action().await;
			}
		});
		debouncer
	}

	pub(crate) fn trigger(&self) {
		if !self.pending.swap(true, Ordering::SeqCst) {
			self.notify.notify_one();
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
		time::Duration,
	};

	use super::Debouncer;

	fn counting(delay: Duration) -> (Debouncer, Arc<AtomicUsize>) {
		let count = Arc::new(AtomicUsize::new(0));
		let counter = count.clone();
		let debouncer = Debouncer::spawn(delay, move || {
			let counter = counter.clone();
			async move {
				counter.fetch_add(1, Ordering::SeqCst);
			}
		});
		(debouncer, count)
	}

	#[tokio::test]
	async fn coalesces_a_burst_into_one_run() {
		let (debouncer, count) = counting(Duration::from_millis(100));
		for _ in 0..20 {
			debouncer.trigger();
		}
		tokio::time::sleep(Duration::from_millis(30)).await;
		assert_eq!(count.load(Ordering::SeqCst), 0);
		debouncer.trigger();
		tokio::time::sleep(Duration::from_millis(250)).await;
		assert_eq!(count.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn runs_again_for_a_later_trigger() {
		let (debouncer, count) = counting(Duration::from_millis(50));
		debouncer.trigger();
		tokio::time::sleep(Duration::from_millis(200)).await;
		debouncer.trigger();
		tokio::time::sleep(Duration::from_millis(200)).await;
		assert_eq!(count.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn does_nothing_without_a_trigger() {
		let (_debouncer, count) = counting(Duration::from_millis(10));
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(count.load(Ordering::SeqCst), 0);
	}
}
