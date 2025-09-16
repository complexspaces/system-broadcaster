//! A end-to-end test of the system's implementation of notifications.

#[cfg(target_os = "macos")]
use core_foundation::runloop::{self, CFRunLoop};
use futures_util::StreamExt;
use std::time::Duration;
use system_broadcaster::{Listener, Sender};

fn notification_name() -> &'static str {
    let id: String = (0..32)
        .map(|_| {
            char::from(rand::Rng::sample(
                &mut rand::rng(),
                rand::distr::Alphanumeric,
            ))
        })
        .collect();
    Box::leak(format!("test_foobar-{id}").into_boxed_str())
}

fn main() {
    // On macOS, notifications are dispatched to the main thead (even if they are dispatched elsewhere from there),
    // so we need to drive its runloop (which happens automatically in the desktop app).
    {
        // XXX: This is kind of hacky, but `ThreadId` is too opaque currently. This could be removed
        // if it becomes problematic in future Rust versions though since its only to smoke-check this
        // test.
        let current_id = format!("{:?}", std::thread::current().id());
        let current_id = current_id
            .trim_start_matches("ThreadId(")
            .trim_end_matches(')');
        assert_eq!(current_id.parse::<u64>().unwrap(), 1);
    }

    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let example_notification_name = notification_name();

    // XXX: We must use a multi-threaded runtime here so that, on macOS, the future can run while the runloop of
    // the main thead drives the callbacks (which also blocks the rest of the thread.)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .enable_io()
        .build()
        .unwrap();

    // XXX: This is inside a `block_on` call because some implementations can only
    // create a listener inside an executor context.
    let mut listener = rt.block_on(async {
        // Smoke check that its fine to have multiple listeners running in the same process
        // both on the same notification and on unrelated ones. In both those cases, they are
        // independent of the one we're using and don't interfere.
        std::mem::forget(Listener::create(example_notification_name));
        std::mem::forget(Listener::create("foobar_123"));

        Listener::create(example_notification_name).unwrap()
    });

    let msg_1 = b"test_content";
    let msg_2 = b"test_content_2";

    // This doesn't need to be on a background thread, but is for this example.
    std::thread::spawn(|| {
        let mut sender = Sender::create(example_notification_name).unwrap();
        sender.send(msg_1);
        sender.send(msg_2);
        log::info!("sent notifications");
    });

    let waiter = rt.spawn(async move {
        let mut listener_temp = Box::pin(listener.listen());
        let msg_1 = listener_temp.next().await.unwrap();
        log::info!("received first notification");
        let msg_2 = listener_temp.next().await.unwrap();
        log::info!("received second notification");
        drop(listener_temp);

        (listener, msg_1, msg_2)
        // Return as soon as we've heard about both messages.
    });

    // Due to how notifications are delivered from the system on macOS,
    // we need to independently drive the main thread's runloop for this test.
    #[cfg(target_os = "macos")]
    loop {
        CFRunLoop::run_in_mode(
            // SAFETY: This static is always available at runtime, and
            // only the system dereferences the pointer.
            unsafe { runloop::kCFRunLoopDefaultMode },
            Duration::from_millis(100),
            false,
        );

        if waiter.is_finished() {
            break;
        }
    }

    // Ensure that all messages were received, and that no other
    // parallel listeners produced messages we can see.
    //
    // At this point, we know that any other listeners have dispatched their events
    // since we received our own too.
    rt.block_on(async move {
        let (mut listener, received_msg1, received_msg2) = waiter.await.unwrap();
        let mut listener = std::pin::pin!(listener.listen());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.next())
                .await
                .is_err()
        );

        assert_eq!(received_msg1, msg_1);
        assert_eq!(received_msg2, msg_2);
    });

    log::info!("All messages received!");
}
