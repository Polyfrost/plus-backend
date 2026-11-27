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

	#[tokio::test]
	async fn a_burst_runs_once_after_the_quiet_period_and_a_later_trigger_runs_again() {
		let count = Arc::new(AtomicUsize::new(0));
		let counter = count.clone();
		let debouncer = Debouncer::spawn(Duration::from_millis(100), move || {
			let counter = counter.clone();
			async move {
				counter.fetch_add(1, Ordering::SeqCst);
			}
		});
		let runs = || count.load(Ordering::SeqCst);
		tokio::time::sleep(Duration::from_millis(150)).await;
		assert_eq!(runs(), 0);
		for _ in 0..20 {
			debouncer.trigger();
		}
		tokio::time::sleep(Duration::from_millis(30)).await;
		assert_eq!(runs(), 0);
		debouncer.trigger();
		tokio::time::sleep(Duration::from_millis(250)).await;
		assert_eq!(runs(), 1);
		debouncer.trigger();
		tokio::time::sleep(Duration::from_millis(250)).await;
		assert_eq!(runs(), 2);
	}
}
