use super::InvalidNameError;
use std::{ffi::CString, io, os::fd::AsRawFd, ptr};
use tokio::io::unix::AsyncFd;

const COMMON_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_CLOEXEC;

struct QueueDescriptor(libc::mqd_t);

fn handle_result<T: Into<libc::c_long> + Copy>(result: T) -> Result<T, io::Error> {
    if result.into() < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

impl QueueDescriptor {
    fn from_result(result: libc::c_int) -> Result<Self, io::Error> {
        handle_result(result).map(Self)
    }
}

impl AsRawFd for QueueDescriptor {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

impl Drop for QueueDescriptor {
    fn drop(&mut self) {
        // SAFETY: We have exclusive access to `self` right now, so the
        // following is true:
        // - In the listenter, we know there is nothing waiting to or actively using the
        // descriptor. Therefore, we can close this descriptor because there's nothing
        // waiting for it to be readable and use it. Tokio still has it registered, but
        // it will be un-registered during our parent's drop as well. Note that this depends
        // on `QueueDescriptor` remaining private.
        //
        // - In the `sender`, nothing is actively using the queue.
        unsafe { libc::mq_close(self.0) };
    }
}

/// Validates a queue name based on the requirements defined in [mq_overview].
///
/// [mq_overview]: <https://linux.die.net/man/7/mq_overview>
fn create_name(notification_name: &'static str) -> Result<CString, InvalidNameError> {
    const NAME_PREFIX: &str = "sys-broadc-";

    if notification_name.is_empty() {
        return Err(InvalidNameError::Empty);
    }

    #[allow(clippy::as_conversions)] // This is a lossless conversion
    if notification_name.len() >= (libc::PATH_MAX as usize - (1 + NAME_PREFIX.len())) {
        return Err(InvalidNameError::TooLong);
    }

    if notification_name.contains('/') {
        return Err(InvalidNameError::ContainsSlash);
    }

    let name = std::iter::once(b'/')
        .chain(NAME_PREFIX.bytes())
        .chain(notification_name.bytes())
        .chain(Some(0))
        .collect();

    CString::from_vec_with_nul(name).map_err(|_| InvalidNameError::ContainsNul)
}

pub(super) struct Listener {
    queue: AsyncFd<QueueDescriptor>,
    queue_name: CString,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: `queue_name` is the same name this listener was created with
        // and is a valid pointer. The queue will be removed, and then completely
        // go away once every process has closed any open descriptors.
        //
        // Note: No errors are possible here because we're the same process that
        // opened the queue and kept the same `name` pointer.
        let _ = unsafe { libc::mq_unlink(self.queue_name.as_ptr()) };
    }
}

impl Listener {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        let name = create_name(notification_name)?;

        // SAFETY: This structure only contains values that have a valid zero bitpattern.
        //
        // Note: This needs manually zeroed-then-assign because it contains private padding fields.
        let mut attrs: libc::mq_attr = unsafe { std::mem::zeroed() };
        // `mq_flags` and `mq_curmsgs` are ignored when opening a queue
        #[allow(clippy::as_conversions)]
        {
            attrs.mq_maxmsg = super::MSG_LIMIT as i64;
            attrs.mq_msgsize = super::MAX_MSG_SIZE as i64;
        }

        // SAFETY: `name` is a valid pointer to a NUL terminated string (and has been validated),
        // the open flags passed in are valid and correct, the creation mask only allows this user
        // to access the queue (rw--------), and `attrs` is valid.
        let wrapper = QueueDescriptor::from_result(unsafe {
            libc::mq_open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CREAT | COMMON_FLAGS,
                0o600,
                &mut attrs,
            )
        })
        .unwrap();

        // A listener only receives messages, so we're only interested in knowing when we can read.
        let queue = AsyncFd::with_interest(wrapper, tokio::io::Interest::READABLE).unwrap();

        Ok(Self {
            queue,
            queue_name: name,
        })
    }

    pub(super) fn listen(&mut self) -> impl futures_util::Stream<Item = Vec<u8>> + '_ {
        futures_util::stream::unfold(&self.queue, |queue| async move {
            // Note: `mq_receive` will fail if this is less than the max message size set when
            // creating the queue.
            let mut buffer: Vec<u8> = Vec::with_capacity(super::MAX_MSG_SIZE);

            // Wait for a message to be sent on the queue.
            // Note: This unwrap can only fail if the runtime is shutting down.
            let mut ready = queue.readable().await.unwrap();

            // SAFETY: `queue` is a valid message queue descriptor, `buffer` is writable,
            // `msg_len` is correct, and `NULL` is a valid parameter for the priority output.
            //
            // The length of the buffer only needs to be >= the message length for a successful
            // operation, and the kernel validates the buffer's size too which prevents overflow.
            let read = match handle_result(unsafe {
                // This cast can never truncate.
                #[allow(clippy::as_conversions)]
                let raw = libc::mq_receive(
                    queue.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.capacity(),
                    ptr::null_mut(),
                ) as i64;
                raw
            }) {
                Ok(r) => r,
                Err(e) => {
                    log::error!("failed to read queue message: {e}");
                    return None;
                }
            };

            // XXX: This has a very small chance to be affected by spurious failures
            // due to timeouts or signal handlers.

            // After we've read a message sucessfully, tell Tokio that we've handled
            // the readability event and that it needs to start waiting again.
            ready.clear_ready();

            // This cast is always lossless because we know the value is positive.
            #[allow(clippy::as_conversions)]
            // SAFETY: The system wrote `read` bytes into the buffer, so the range has been initialized.
            unsafe {
                buffer.set_len(read as usize)
            };
            // Remove any excess capacity that the message didn't need.
            buffer.shrink_to_fit();

            Some((buffer, queue))
        })
    }
}

pub(super) struct Sender {
    notification_name: CString,
}

impl Sender {
    pub(super) fn create(notification_name: &'static str) -> Result<Self, InvalidNameError> {
        let notification_name = create_name(notification_name)?;
        Ok(Self { notification_name })
    }

    pub(super) fn send(&mut self, content: &[u8]) {
        // SAFETY: `name` is a valid NUL terminated string (and has been validated)
        // and the open flags are a valid combination.
        let queue = match QueueDescriptor::from_result(unsafe {
            libc::mq_open(
                self.notification_name.as_ptr().cast(),
                libc::O_WRONLY | COMMON_FLAGS,
            )
        }) {
            Ok(queue) => queue,
            // In order to match the one-off broadcast behavior of the other platforms,
            // we simply bail out here if nothing has started listening yet.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return,
            Err(err) => panic!("failed to connect to queue: {}", err),
        };

        // SAFETY: `queue` is a valid descriptor, `content` points to a readable byte buffer
        // and the length matches, and priority can be any value.
        //
        // Note that `content`'s size is limited to the max message size defined when opening the queue,
        // but it will return an error instead of causing unsafety in that case.
        match handle_result(unsafe {
            libc::mq_send(queue.as_raw_fd(), content.as_ptr().cast(), content.len(), 5)
        }) {
            Ok(_) => {}
            Err(err) if err.raw_os_error() == Some(libc::EMSGSIZE) => {
                log::warn!(
                    "dropping queue message becuase it was larger then the receiver will accept"
                );
            }
            Err(err) => {
                log::error!("failed to send notification: {}", err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_MSG_SIZE, test_utils::get_notification_name};
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn sender_skips_if_no_receiver_exists() {
        let mut sender = Sender::create(get_notification_name()).unwrap();
        sender.send(&[0, 1, 2, 3]);
    }

    #[test]
    fn names_correctly_validated() {
        #[allow(clippy::as_conversions)]
        let too_long_name = Box::leak("a".repeat((libc::PATH_MAX + 5) as usize).into_boxed_str());

        for (name, expected) in [
            ("", Err(InvalidNameError::Empty)),
            (too_long_name, Err(InvalidNameError::TooLong)),
            ("name/with/slashes", Err(InvalidNameError::ContainsSlash)),
            ("name_with_nul\0", Err(InvalidNameError::ContainsNul)),
            (
                "good_name",
                Ok(CString::new("/sys-broadc-good_name").unwrap()),
            ),
        ] {
            assert_eq!(create_name(name), expected)
        }
    }

    #[tokio::test]
    async fn sending_oversized_msg_doesnt_crash() {
        let notification_name = get_notification_name();

        tokio::spawn(async {
            let mut listener = Listener::create(notification_name).unwrap();
            let mut listener = std::pin::pin!(listener.listen());
            // This is never expected to resolve.
            let _msg = listener.next().await;
        });

        let mut sender = Sender::create(notification_name).unwrap();
        let payload: Vec<u8> = std::iter::repeat_n(5, MAX_MSG_SIZE + 10).collect();
        sender.send(&payload);
    }
}
