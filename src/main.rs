mod discord_handler;
mod repost_checker;

use discord_handler::Handler;
use repost_checker::RepostChecker;
use serenity::prelude::*;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("discord_repost"))
        .init();

    let token =
        std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN environment variable must be set");
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let repost_checker = Arc::new(
        RepostChecker::new()
            .await
            .expect("Failed to initialize database"),
    );
    let mut client = Client::builder(token, intents)
        .event_handler(Handler { repost_checker })
        .await
        .expect("Error creating client");

    log::info!("Starting bot...");

    // start listening for events by starting a single shard
    if let Err(err) = client.start().await {
        log::error!("An error occurred while running the client: {:?}", err);
    }
}
