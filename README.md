# system-broadcaster

This library provides a set of senders and listeners that provide a MPMC (multi-producer, multi-consumer) channel which works across process boundaries
on the same local system and inside the same user context. 

This logic can be helpful for a number of reasons, such as a lightweight/silent out-of-band transport to signal another
cooperating process by the same developer to look somewhere else for data or to setup a more complicated or reliable communication channel.

## Platform Support
At this time, `system-broadcaster` supports Linux and macOS. Windows support is planned to be added in the near future.

## Properties

While using this crate, there are several properties to be aware of:
- **Lossy**: The messages sent using this crate may or may not be received by listeners. 
    - If its important to receive a payload, senders should implement confirmation with the receiver. Operating systems may coalesce sent messages, for example.
- **Insecure**: There is no sender authentication, or built-in transport security. 
    - It should be assumed any process, legitimate or not, may see sent message contents and that listeners may receive garbage/untrusted data.
- **Payload size**: It is not advised to send large (multi-KB/MB) messages through these channels.
    - This may introduce slowness or unreliability on some operating systems.

## License

This crate is licensed under the [MIT license](./LICENSE-MIT).