# Repost Checker

Just your regular annoying bot that will notify when someone reposts a link on
Discord and give credit to the people who did it first!

## Database

The bot uses a SQLite database (`reposts.db`) to store the reposted links.

## Configuration

The bot is configured using a `discord.yaml` file. You can use the provided `discord.yaml.example` as a starting point.

## Migration

If you have been using a previous version of this bot, you can migrate your old `checkpoint.json` file to the new SQLite database by running the following command:

```sh
cargo run --bin migration
```

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
