# waybar-net-daemon
A daemon whose purpose is to make http requests, cache them, and allow them to be fetched over a local unix socket. The intent of this is move all network handling for my waybar components into one high performance area so that I don't have multiple binaries cold starting and creating a new connection with all that overhead.

## Why
This largely exists for reducing the overhead of cold starting clients as waybar runs every configured binary at the configured interval when looking for information on custom data elements and it is far cheaper computationally speaking to request this info over a unix socket with a long running daemon that do these requests independently. It's also easier to audit that the data returned is what we expect and makes error handling\network caching a lot simpler which is exceptionally important because a malformed message from a binary for waybar just removes the element from the bar.

## How this works
The configuration file `waybar-net-daemon.yaml` is read and each http request configured is made at the configured interval. If the user requests a key from the daemon then the respective state will be returned.

## Build\use
Build the project with `cargo build --release` and update `waybar-net-daemon.yaml` as appropriate.
