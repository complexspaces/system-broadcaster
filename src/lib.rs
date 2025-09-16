#![doc = include_str!("../README.md")]
#![warn(
    clippy::as_conversions,
    clippy::undocumented_unsafe_blocks,
    missing_docs
)]

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::{Listener as ListenerImp, Sender as SenderImp};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::{Listener as ListenerImp, Sender as SenderImp};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("system-broadcaster is unsupported on this platform");

/// The number of messages that may be queued for receiving at once.
const MSG_LIMIT: usize = 10;

/// The max message data size that might be sent on an IPC backend.
///
/// Values larger then this may never be seen by receivers on some platforms.
//
// XXX: 1KB seems like a reasonable cap for now, but this could be made configurable
// in the future if needed.
const MAX_MSG_SIZE: usize = 1000;

/// An error that can occur when trying to parse a notification name
/// into a platform channel identifier.
#[allow(dead_code)] // Not all platforms use the error variants.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum InvalidNameError {
    /// The name was too short for the platform to use.
    Empty,
    /// The name was too long for the platform to use.
    TooLong,
    /// The name contained a seperator character that wasn't allowed.
    ContainsSlash,
    /// The name contained an interior NUL.
    ContainsNul,
}

/// An inter-process listener for notifications.
///
/// Notifications are filtered and received based off an identifier, and each [Listener]
/// can only listen for one at a time.
///
/// It is valid to have multiple listeners for a single notification.
pub struct Listener {
    backend: ListenerImp,
}

impl Listener {
    /// Creates a new listener and begins to handle any notifications received.
    ///
    /// `notification_name` should usually be hardcoded, since both process' need to know
    /// the name.
    ///
    /// All notifications are internally queued, up until an internal size limit.
    pub fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        let backend = ListenerImp::create(notification_name)?;
        Ok(Self { backend })
    }

    /// Returns a listener [Stream] that can be used to receive
    /// notifications from other [Sender] instances, even in other processes.
    ///
    /// [Stream]: futures_util::Stream
    pub fn listen(&mut self) -> impl futures_util::Stream<Item = Vec<u8>> + '_ {
        self.backend.listen()
    }
}

/// An inter-process sender for arbitrary notifications to a [Listener].
pub struct Sender {
    backend: SenderImp,
}

impl Sender {
    /// Creates a new [Sender] that can be used to send notifications to a corresponding
    /// [Listener] for the same notification name, even in another process.
    pub fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        let backend = SenderImp::create(notification_name)?;
        Ok(Self { backend })
    }

    /// Sends a notification containing `content` for the specified notification name.
    ///
    /// ### WARNING: This data is completely unsecured, and could be read by any process on the system
    /// Do **not** send secrets with this function.
    ///
    /// There is no guarantee that a listener will receive this message, so this transport should
    /// be treated as lossy too.
    pub fn send(&mut self, content: &[u8]) {
        self.backend.send(content)
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod test_utils {
    pub(super) fn get_notification_name() -> &'static str {
        let id: String = (0..32)
            .map(|_| {
                char::from(rand::Rng::sample(
                    &mut rand::rng(),
                    rand::distr::Alphanumeric,
                ))
            })
            .collect();

        Box::leak(format!("system-broadcaster-test{id}").into_boxed_str())
    }
}

// macOS can not be tested in the same way because it doesn't work well in the
// Cargo test harness currently.
#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[tokio::test]
    async fn valid_to_send_empty_payload() {
        let notification_name = test_utils::get_notification_name();

        let mut listener = Listener::create(notification_name).unwrap();
        let incoming_msg = tokio::spawn(async move {
            let mut listener = std::pin::pin!(listener.listen());
            listener.next().await.unwrap()
        });

        let msg = &[];
        let mut sender = Sender::create(notification_name).unwrap();
        sender.send(msg);

        let received_msg = incoming_msg.await.unwrap();
        assert_eq!(msg.as_slice(), received_msg);
    }
}
