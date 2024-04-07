# Repost Checker

Just your regular annoying bot that will notify when someone reposts a link on
Discord and give credit to the people who did it first!

## Build

This is run on a Raspberry Pi 4 but the Pi is too weak to build this so it's
cross compiled.

```sh
# Add Rust toolchain
rustup target add aarch64-unknown-linux-gnu

# Add GCC
sudo apt-get install gcc-aarch64-linux-gnu

# Build for Pi
cargo build --target=aarch64-unknown-linux-gnu
```
