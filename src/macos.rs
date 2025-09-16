use super::InvalidNameError;
use core_foundation::{
    base::{Boolean, CFType, CFTypeID, CFTypeRef, TCFType},
    data::CFData,
    declare_TCFType,
    dictionary::{CFDictionary, CFDictionaryRef},
    impl_TCFType,
    string::{CFString, CFStringRef},
};
use futures_util::Stream;
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    ptr::{self, NonNull},
    sync::{Arc, Weak},
};
use tokio::sync::mpsc;

#[repr(C)]
pub struct __CFNotificationCenter(c_void);
type CFNotificationCenterRef = *const __CFNotificationCenter;

declare_TCFType!(CFNotificationCenter, CFNotificationCenterRef);
impl_TCFType!(
    CFNotificationCenter,
    CFNotificationCenterRef,
    CFNotificationCenterGetTypeID
);

// SAFETY: This is a global object owned by the system, which we can get
// references to on any thread.
unsafe impl Send for CFNotificationCenter {}

type CFNotificationName = CFStringRef;
#[allow(non_snake_case)]
type CFNotificationCallback = extern "C" fn(
    center: CFNotificationCenterRef,
    observer: *mut c_void,
    name: CFNotificationName,
    object: *const c_void,
    userInfo: CFDictionaryRef,
);

unsafe extern "C" {
    fn CFNotificationCenterGetTypeID() -> CFTypeID;
    fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
    fn CFNotificationCenterAddObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
        callBack: CFNotificationCallback,
        name: CFStringRef,
        object: *const c_void,
        suspensionBehavior: CFNotificationSuspensionBehavior,
    );
    fn CFNotificationCenterRemoveEveryObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
    );
    fn CFNotificationCenterPostNotification(
        center: CFNotificationCenterRef,
        name: CFNotificationName,
        object: *const c_void,
        userInfo: CFDictionaryRef,
        deliverImmediately: Boolean,
    );
}

#[repr(transparent)]
struct CFNotificationSuspensionBehavior(isize);

#[allow(non_upper_case_globals, dead_code)]
impl CFNotificationSuspensionBehavior {
    /// The server will not queue any notifications with this name and object while the process/app is in the background.
    const CFNotificationSuspensionBehaviorDrop: Self = Self(1);
    /// The server will only queue the last notification of the specified name and object; earlier notifications are dropped.
    const CFNotificationSuspensionBehaviorCoalesce: Self = Self(2);
    /// The server will hold all matching notifications until the queue has been filled (queue size determined by the server) at which point the server may flush queued notifications.
    const CFNotificationSuspensionBehaviorHold: Self = Self(3);
    /// The server will deliver notifications matching this registration whether or not the process is in the background.  When a notification with this suspension behavior is matched, it has the effect of first flushing any queued notifications.
    const CFNotificationSuspensionBehaviorDeliverImmediately: Self = Self(4);
}

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
extern "C" fn notification_callback(
    _center: CFNotificationCenterRef,
    observer: *mut c_void,
    _name: CFNotificationName,
    _object: *const c_void,
    userInfo: CFDictionaryRef,
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
    if userInfo.is_null() {
        send_message(Vec::new());
        return;
    }

    // SECURITY: While the documentation says you must provide a dictionary object, this isn't enforced at runtime. Any valid (not-freed)
    // runtime object may be passed instead. To defend against malicious notification senders, we validate that its truly a dictionary and
    // that the contents are what we expect.
    //
    // SAFETY: `userInfo` is a valid runtime object that we can attempt to downcast.
    let Some(details) =
        (unsafe { CFType::wrap_under_get_rule(userInfo.cast()).downcast_into::<CFDictionary>() })
    else {
        return;
    };

    fn wrap_dictionary_pair(pair: (CFTypeRef, CFTypeRef)) -> (CFType, CFType) {
        // SAFETY: Directly called on valid pointers, and isn't exposed
        // to other code to call.
        let k = unsafe { CFType::wrap_under_get_rule(pair.0) };
        // SAFETY: See above
        let v = unsafe { CFType::wrap_under_get_rule(pair.1) };
        (k, v)
    }

    let (keys, values) = details.get_keys_and_values();
    for (k, v) in keys.into_iter().zip(values).map(wrap_dictionary_pair) {
        // If any messages don't conform to the dictionary scheme expected, we drop and ignore them.
        let Some(key) = k.downcast_into::<CFString>() else {
            break;
        };

        // Ignore any key/value pairs that don't match the ones we used to send.
        if key != SENDER_KEY_NAME {
            continue;
        }

        let Some(value) = v.downcast_into::<CFData>() else {
            break;
        };

        #[allow(clippy::as_conversions)]
        if value.len() as usize > super::MAX_MSG_SIZE {
            break;
        }

        send_message(value.to_vec());
    }
}

pub(super) struct Listener {
    inner: CFNotificationCenter,
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
        // SAFETY: `center` is a notification center object, and the same
        // one the observer was added onto, and `observer_ref` is the same pointer
        // originally provided when the observer was added.
        unsafe {
            CFNotificationCenterRemoveEveryObserver(
                self.inner.as_concrete_TypeRef(),
                // Note: We know this will remove the notification handler correctly
                // because we only have one add operation and keep the pointer
                // consistent.
                self.observer_ref.as_ptr().cast(),
            );
        };

        // SAFETY: After the observer/callback is removed, we know the callback won't trigger
        // again and try to use the sender again. Therefore, it can be safely freed.
        unsafe { Weak::from_raw(self.observer_ref.as_ptr()) };
    }
}

impl Listener {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        // SAFETY: Every application on macOS can access the distributed notification center, and there
        // are no invariants to uphold.
        let listener = unsafe {
            CFNotificationCenter::wrap_under_get_rule(CFNotificationCenterGetDistributedCenter())
        };

        let notification_name = CFString::from_static_string(notification_name);

        let (tx, rx) = mpsc::channel(super::MSG_LIMIT);
        let tx = Arc::new(tx);
        let observer_tx = Weak::into_raw(Arc::downgrade(&tx));

        // SAFETY: All of the parameters are valid:
        // - `listener` is a valid reference to a notification center
        // - `observer_tx` is a valid pointer to Weak sender reference that was just made.
        // - `notification_callback` has the correct type signature.
        // - `notification_name` is a valid CFString pointer.
        // - NULL is a valid parameter for `object` when unused.
        // - `suspensionBehavior` uses a valid parameter.
        unsafe {
            CFNotificationCenterAddObserver(
                listener.as_concrete_TypeRef(),
                // Note: `observer_tx` is a callback context pointer, but its also used for keying
                // observers added to the center.
                observer_tx.cast(),
                notification_callback,
                notification_name.as_concrete_TypeRef(),
                // Note: Object MUST be `null` because its intended to be used across processes, so we can't
                // have the system filter notification handling using it.
                ptr::null(),
                CFNotificationSuspensionBehavior::CFNotificationSuspensionBehaviorDeliverImmediately,
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
    inner: CFNotificationCenter,
    notification_name: CFString,
}

impl Sender {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        let notification_name = CFString::from_static_string(notification_name);

        // SAFETY: Every application on macOS can access the distributed notification center, and there
        // are no invariants to uphold.
        let sender = unsafe {
            CFNotificationCenter::wrap_under_get_rule(CFNotificationCenterGetDistributedCenter())
        };

        Ok(Self {
            inner: sender,
            notification_name,
        })
    }

    pub(super) fn send(&mut self, content: &[u8]) {
        // Note: All the values inside this dictionary must only contain plist-compatible
        // types.
        let content = CFDictionary::from_CFType_pairs(&[(
            CFString::from_static_string(SENDER_KEY_NAME),
            CFData::from_buffer(content),
        )]);

        // SAFETY: All of the parameters are valid:
        // - `center` uses a valid pointer to a notification center
        // - `notification_name` is a valid CFString pointer
        // - NULL is a valid parameter for `object` when unused.
        // - `userInfo` is a valid dictionary with the correct inner types.
        // - `deliverImmediately` is a boolean
        unsafe {
            CFNotificationCenterPostNotification(
                self.inner.as_concrete_TypeRef(),
                self.notification_name.as_concrete_TypeRef(),
                ptr::null(),
                content.as_concrete_TypeRef().cast(),
                Boolean::from(true),
            )
        };
    }
}
