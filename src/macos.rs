use super::InvalidNameError;
use futures_util::Stream;
use objc2_core_foundation::{
    CFData, CFDictionary, CFNotificationCenter, CFNotificationName,
    CFNotificationSuspensionBehavior, CFRetained, CFString, CFType,
};
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    ptr::{self, NonNull},
    sync::{Arc, Weak},
};
use tokio::sync::mpsc;

const SENDER_KEY_NAME: &str = "sys-broadcaster-payload";
type CallbackSender = mpsc::Sender<Vec<u8>>;

struct CallbackReceiver {
    inner: mpsc::Receiver<Vec<u8>>,
}

impl Stream for CallbackReceiver {
    type Item = Vec<u8>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

// Note: The `userInfo` and `object` pointers are untrusted. Any process on the system can trigger this callback.
//
// `observer` and `name` are safe, since they come from this process or directly from the system.
#[allow(non_snake_case)]
extern "C-unwind" fn notification_callback(
    _center: *mut CFNotificationCenter,
    observer: *mut c_void,
    _name: *const CFNotificationName,
    _object: *const c_void,
    userInfo: *const CFDictionary,
) {
    let sender: ManuallyDrop<Weak<CallbackSender>> =
        // SAFETY: This callback will only be called when the listener owning `Observer`
        // has not yet been dropped. The `Weak` is only invalidated _after_ the callback handler is removed,
        // so every callback has a valid pointer to work with.
        //
        // Additionally, we never free the `Weak` from inside the callback.
        unsafe { ManuallyDrop::new(Weak::from_raw(observer.cast())) };

    // If the listener has been dropped in the middle here, we can't do anything.
    let Some(sender) = sender.upgrade() else {
        return;
    };

    let send_message = |msg| {
        // We aren't concerned with the closed case, since the channel can't be dropped when this callback
        // is running.
        if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send(msg) {
            log::warn!("callback queue was too full, dropping notification")
        }
    };

    // `userInfo` may be NULL in some cases, notably from process' that are inside
    // the default app sandbox.
    let Some(userInfo) = NonNull::new(userInfo.cast_mut()) else {
        send_message(Vec::new());
        return;
    };

    // SECURITY: While the documentation says you must provide a dictionary object, this isn't enforced at runtime. Any valid (not-freed)
    // runtime object may be passed instead. To defend against malicious notification senders, we validate that its truly a dictionary and
    // that the contents are what we expect.
    //
    // SAFETY: `userInfo` is a valid runtime object that we can attempt to downcast.
    let Ok(details) = (unsafe { CFRetained::retain(userInfo).downcast::<CFDictionary>() }) else {
        return;
    };
    // SAFETY: All dicts passed as `userInfo` must be XML compatible, so keys are unconditionally strings.
    let details = unsafe { details.cast_unchecked::<CFString, CFType>() };

    // If any messages don't conform to the dictionary scheme expected, we drop and ignore them.
    let (keys, values) = details.to_vecs();
    for (key, v) in keys.into_iter().zip(values) {
        // Ignore any key/value pairs that don't match the ones we used to send.
        if key.to_string() != SENDER_KEY_NAME {
            continue;
        }

        let Ok(value) = v.downcast::<CFData>() else {
            break;
        };

        if value.len() > super::MAX_MSG_SIZE {
            break;
        }

        send_message(value.to_vec());
    }
}

pub(super) struct Listener {
    inner: CFRetained<CFNotificationCenter>,
    // The backing allocation for the `observer`.
    _sender: Arc<CallbackSender>,
    // Created via `Weak::into_raw`.
    observer_ref: NonNull<CallbackSender>,
    receiver: CallbackReceiver,
}

// SAFETY: Everything inside `Listener` is safe to send across threads,
// and the raw pointer comes from an `Arc`, and is therefore also threadsafe.
unsafe impl Send for Listener {}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: `observer_ref` is the same pointer originally provided when the observer was added.
        unsafe {
            // Note: We know this will remove the notification handler correctly
            // because we only have one add operation and keep the pointer
            // consistent.
            self.inner
                .remove_every_observer(self.observer_ref.as_ptr().cast());
        };

        // SAFETY: After the observer/callback is removed, we know the callback won't trigger
        // again and try to use the sender again. Therefore, it can be safely freed.
        unsafe { Weak::from_raw(self.observer_ref.as_ptr()) };
    }
}

impl Listener {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        // Every application on macOS can access the distributed notification center.
        let listener = CFNotificationCenter::distributed_center().unwrap();

        let notification_name = CFString::from_static_str(notification_name);

        let (tx, rx) = mpsc::channel(super::MSG_LIMIT);
        let tx = Arc::new(tx);
        let observer_tx = Weak::into_raw(Arc::downgrade(&tx));

        // SAFETY: All of the parameters are valid:
        // - `observer_tx` is a valid pointer to Weak sender reference that was just made.
        // - `notification_callback` has the correct type signature.
        // - `notification_name` is a valid CFString pointer.
        // - NULL is a valid parameter for `object` when unused.
        // - `suspensionBehavior` uses a valid parameter.
        unsafe {
            listener.add_observer(
                // Note: `observer_tx` is a callback context pointer, but its also used for keying
                // observers added to the center.
                observer_tx.cast(),
                Some(notification_callback),
                Some(&notification_name),
                // Note: Object MUST be `null` because its intended to be used across processes, so we can't
                // have the system filter notification handling using it.
                ptr::null(),
                CFNotificationSuspensionBehavior::DeliverImmediately,
            )
        }

        Ok(Self {
            inner: listener,
            _sender: tx,
            // SAFETY: We just leaked this `Weak` and haven't called any drop
            // handlers yet.
            observer_ref: unsafe { NonNull::new_unchecked(observer_tx.cast_mut()) },
            receiver: CallbackReceiver { inner: rx },
        })
    }

    pub(super) fn listen(&mut self) -> impl Stream<Item = Vec<u8>> + '_ {
        &mut self.receiver
    }
}

pub(super) struct Sender {
    inner: CFRetained<CFNotificationCenter>,
    notification_name: CFRetained<CFString>,
}

impl Sender {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        // Every application on macOS can access the distributed notification center.
        let sender = CFNotificationCenter::distributed_center().unwrap();

        let notification_name = CFString::from_static_str(notification_name);

        Ok(Self {
            inner: sender,
            notification_name,
        })
    }

    pub(super) fn send(&mut self, content: &[u8]) {
        // Note: All the values inside this dictionary must only contain plist-compatible types.
        let content = CFDictionary::from_slices(
            &[&*CFString::from_static_str(SENDER_KEY_NAME)],
            &[&*CFData::from_bytes(content)],
        );

        // SAFETY: All of the parameters are valid:
        // - `notification_name` is a valid CFString pointer
        // - NULL is a valid parameter for `object` when unused.
        // - `userInfo` is a valid dictionary with the correct inner types.
        // - `deliverImmediately` is a boolean
        unsafe {
            self.inner.post_notification(
                Some(&self.notification_name),
                ptr::null(),
                Some(content.as_opaque()),
                true,
            )
        };
    }
}
