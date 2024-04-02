use serde::{Deserialize, Serialize};
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};

const DISCORD_CONFIG: &str = include_str!("../discord.yaml");
const CHECKPOINT_FILE: &str = "checkpoint.json";

/// DiscordConfig represents all the configuration required for Discord.
#[derive(Debug, Clone, Deserialize)]
struct DiscordConfig {
    token: String,
    always_enabled_urls: Vec<String>,
}

type Reposts = HashMap<String, Vec<(UserId, chrono::DateTime<chrono::Local>)>>;

#[derive(Serialize, Deserialize)]
struct RepostChecker {
    reposts: Reposts,
    #[allow(dead_code)]
    ignore_urls: HashSet<String>,
    #[serde(skip, default = "default_regex")]
    regex_pattern: regex::Regex,
}

fn default_regex() -> regex::Regex {
    regex::Regex::new(r"https?://\S+").expect("Invalid regex")
}

impl RepostChecker {
    fn new() -> Self {
        Self {
            reposts: Default::default(),
            ignore_urls: Default::default(),
            regex_pattern: default_regex(),
        }
    }

    fn load() -> std::io::Result<Self> {
        match std::fs::File::open(CHECKPOINT_FILE) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            _ => Ok(Self::new()),
        }
    }

    fn checkpoint(&self) -> std::io::Result<()> {
        let file = std::fs::File::create(CHECKPOINT_FILE)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, &self)?;
        writer.flush()?;

        Ok(())
    }

    fn extract_urls(&self, source: &str) -> Vec<url::Url> {
        self.regex_pattern
            .find_iter(source)
            .filter_map(|u| url::Url::parse(u.as_str()).ok())
            .collect()
    }

    fn check_repost(&self, u: &url::Url, _channel_id: ChannelId) -> Option<String> {
        let url_str = u.to_string();

        if self.ignore_urls.contains(u.host_str().unwrap_or_default()) {
            return None;
        }

        let repost = match self.reposts.get(&url_str) {
            Some(repost) if !repost.is_empty() => Some(format!(
                "{url_str} har postats {} {}!\n{}",
                repost.len(),
                if repost.len() == 1 {
                    "gång"
                } else {
                    "gånger"
                },
                repost
                    .iter()
                    .map(|(user_id, when)| {
                        let uid = UserId::from(user_id).mention();
                        format!("- {uid} - {}", when.format("%Y-%m-%d %H:%M:%S"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            _ => None,
        };

        repost
    }

    fn add_url(&mut self, u: &url::Url, user_id: UserId, _channel_id: ChannelId) {
        self.reposts
            .entry(u.to_string())
            .or_default()
            .push((user_id, chrono::offset::Local::now()))
    }
}

struct Handler {
    repost_checker: Arc<Mutex<RepostChecker>>,
    always_enabled_urls: Vec<String>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, message: Message) {
        if message.author.bot {
            log::debug!("Skipping message from bot");
            return;
        }

        let messages =
            self.get_messages_to_send(&message.content, &message.author, message.channel_id);

        for message_text in messages {
            let msg = CreateMessage::new()
                .embeds(Default::default())
                .content(message_text);

            if let Err(err) = message.channel_id.send_message(&ctx, msg).await {
                log::error!("Failed to send message: {err:?}");
            }
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let message = reaction.message(&ctx).await.unwrap();
        if !message.author.bot {
            log::debug!("Skipping reaction for non bot message");
            return;
        }

        let response = match &reaction.emoji {
            ReactionType::Unicode(emoji)
                if ["👎", "👎🏻", "👎🏼", "👎🏽", "👎🏿"].contains(&emoji.as_str()) =>
            {
                self.add_site_to_ignore(&message.content)
            }
            _ => return,
        };

        if let Some(response) = response {
            if let Err(err) = message.channel_id.say(&ctx, response).await {
                log::error!("Failed to send message: {err:?}");
            }
        }
    }
}

impl Handler {
    fn get_messages_to_send(
        &self,
        content: &str,
        user: &User,
        channel_id: ChannelId,
    ) -> Vec<String> {
        let mut messages = Vec::new();

        let mut rc = self.repost_checker.lock().unwrap();

        let urls = rc.extract_urls(content);
        if urls.is_empty() {
            return messages;
        }

        for u in urls {
            if let Some(repost) = rc.check_repost(&u, channel_id) {
                messages.push(repost);
            }

            rc.add_url(&u, user.id, channel_id);
        }

        if let Err(err) = rc.checkpoint() {
            log::error!("Failed to checkpoint: {err}");
        }

        messages
    }

    fn add_site_to_ignore(&self, content: &str) -> Option<String> {
        let mut rc = self.repost_checker.lock().unwrap();
        let urls = rc.extract_urls(content);
        let site = urls
            .first()
            .and_then(|u| u.host_str().map(|h| h.to_string()))?;

        if rc.ignore_urls.contains(&site) {
            return None;
        }

        if self.always_enabled_urls.contains(&site) {
            Some(format!("Sorry, det går inte att stänga av {site}"))
        } else {
            rc.ignore_urls.insert(site.clone());
            if let Err(err) = rc.checkpoint() {
                log::error!("Failed to checkpoint: {err}");
            }

            Some(format!("Ok, ska sluta rapportera från {site}"))
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("discord_repost"))
        .init();

    let repost_checker = Arc::new(Mutex::new(RepostChecker::load().unwrap()));

    let config: DiscordConfig = serde_yaml::from_str(DISCORD_CONFIG).expect("Invalid config");
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(config.token, intents)
        .event_handler(Handler {
            repost_checker,
            always_enabled_urls: config.always_enabled_urls,
        })
        .await
        .expect("Error creating client");

    // start listening for events by starting a single shard
    if let Err(err) = client.start().await {
        log::error!("An error occurred while running the client: {:?}", err);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_parse_url() {
        let rc = RepostChecker::new();
        let content = r#"
            Hello,
            https://www.thegithubshop.com/1455317-00-miirr-vacuum-insulated-hatchback-bottle äkta utvecklare
            some more info
            https://www.thegithubshop.com/1543073-00-invertocat-stanley-tumbler
            some complex?
            Här har hon den på Tiny Desk! https://youtu.be/ANPbOxaRIO0?feature=shared
        "#;

        assert_eq!(3, rc.extract_urls(content).len());
    }

    #[test]
    fn test_check_repost() {
        let mut rc = RepostChecker::new();

        let u = url::Url::parse("https://svt.se").unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).is_none());

        rc.add_url(&u, 123.into(), 1u64.into());
        assert!(rc.check_repost(&u, 1u64.into()).is_some());
    }
}
