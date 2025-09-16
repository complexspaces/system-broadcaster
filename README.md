# system-broadcaster

This library provides a [Listener]() and [Sender]() pair that expose a MPMC (multi-producer, multi-consumer) channel which works across process boundaries
on the same local system and inside the same user context. 

This capability can be helpful for a number of reasons, such as a lightweight/silent out-of-band transport to signal another
cooperating process by the same developer to look somewhere else for data or to setup a more complicated/reliable communication channel.

## Platform Support
At this time, `system-broadcaster` supports Linux and macOS. Windows support is planned to be added in the near future.

### macOS

The macOS backend currently requires that the process' main thread is being driven via a platform event loop, such as `[NSApp run]` or `CFRunLoopRun`. 
This limitation may be removed in the future, but is currently needed due to the underlying OS implementation used by this crate. If `system-broadcaster` is used in
a GUI application, no additional changes are needed.

If you plan on using this inside of a non-standard application (such as a Rust-based CLI), your app can spawn an additional thread to handle most logic, such 
as a Tokio runtime (for example). The `main` thread should "block" on `CFRunLoopRun` to drive the event loop. `CFRunLoop` can be accessed or configured
with crates such as [core-foundation](https://docs.rs/core-foundation/latest/core_foundation/runloop/struct.CFRunLoop.html#method.run_current) or [objc2-core-foundation](https://docs.rs/objc2-core-foundation/latest/objc2_core_foundation/struct.CFRunLoop.html#method.run).

## Properties

While using this crate, there are several properties to be aware of:
- **Lossy**: The messages sent using this crate may or may not be received by listeners. 
    - If its important to receive a payload, senders should implement confirmation with the receiver. Operating systems may coalesce sent messages, for example.
- **Insecure**: There is no sender authentication, or built-in transport security. 
    - It should be assumed any process, legitimate or not, may see the contents of sent messages and that listeners may receive garbage/untrusted data.
- **Payload size**: It is not advised to send large (multi-KB/MB) messages through these channels.
    - This may introduce slowness or unreliability on some operating systems.

## License

This crate is licensed under the [MIT license](./LICENSE-MIT).