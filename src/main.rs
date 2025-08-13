use serde::{Deserialize, Serialize};
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};

const CHECKPOINT_FILE: &str = "checkpoint.json";

/// [`DiscordConfig`] represents all the configuration required for Discord.
#[derive(Debug, Clone, Deserialize)]
struct DiscordConfig {
    token: String,
    always_enabled_urls: HashSet<String>,
}

type Reposts = HashMap<String, Vec<(UserId, chrono::DateTime<chrono::Local>)>>;

/// [`RepostChecker`] is the storage of posted URLs, by whom and when it was posted. It's used to
/// check for already posted links.
#[derive(Serialize, Deserialize)]
struct RepostChecker {
    reposts: Reposts,
    ignore_urls: HashSet<String>,
    #[serde(skip, default = "default_regex")]
    regex_pattern: regex::Regex,
    #[serde(default)]
    always_enabled_urls: HashSet<String>,
}

/// The regex used to find URLs. It only cares about anything that starts with the `http` or
/// `https` protocol.
fn default_regex() -> regex::Regex {
    regex::Regex::new(r"https?://\S+").expect("Invalid regex")
}

impl RepostChecker {
    /// Create a new empty [`RepostChecker`] without any reposts or URLs to ignore. Should be used
    /// if there's no existing state.
    fn new(always_enabled_urls: HashSet<String>) -> Self {
        Self {
            reposts: Default::default(),
            ignore_urls: Default::default(),
            regex_pattern: default_regex(),
            always_enabled_urls,
        }
    }

    /// Load an existing state of reposts from a JSON file. If the file doesn't exist it will fall
    /// back to return an empty [`RepostChecker`].
    fn load(always_enabled_urls: HashSet<String>) -> std::io::Result<Self> {
        match std::fs::File::open(CHECKPOINT_FILE) {
            Ok(file) => {
                let mut me: RepostChecker = serde_json::from_reader(file)?;
                me.always_enabled_urls.extend(always_enabled_urls);

                Ok(me)
            }
            _ => Ok(Self::new(always_enabled_urls)),
        }
    }

    /// Write the current state to disk to persist it. Will overwrite any existing file.
    fn checkpoint(&self) -> std::io::Result<()> {
        let file = std::fs::File::create(CHECKPOINT_FILE)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, &self)?;
        writer.flush()?;

        Ok(())
    }

    /// Extract all URLs from a test string. Used to check if a Discord message contains any URLs
    /// that we need to check for repost.
    fn extract_urls(&self, source: &str) -> Vec<url::Url> {
        self.regex_pattern
            .find_iter(source)
            .filter_map(|u| url::Url::parse(u.as_str()).ok())
            .collect()
    }

    /// Check if the passed URL has been posted before. If so, return a message saying how many
    /// times and by which user it's been posted.
    fn check_repost(&self, u: &url::Url, _channel_id: ChannelId) -> Option<String> {
        let url_str = u.to_string();

        if self.ignore_urls.contains(u.host_str().unwrap_or_default()) {
            return None;
        }

        match self.reposts.get(&url_str) {
            Some(repost) if !repost.is_empty() => {
                let seen_count = repost.len();
                let times_text = if seen_count == 1 { "gång" } else { "gånger" };
                let repost_list = repost
                    .iter()
                    .map(|(user_id, when)| {
                        let uid = UserId::from(user_id).mention();
                        format!("- {uid} - {}", when.format("%Y-%m-%d %H:%M:%S"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Some(format!(
                    "{url_str} har postats {} {}!\n{}",
                    seen_count, times_text, repost_list
                ))
            }
            _ => None,
        }
    }

    /// Add a URL as seen by the passed `user_id`.
    fn add_url(&mut self, u: &url::Url, user_id: UserId, _channel_id: ChannelId) {
        self.reposts
            .entry(u.to_string())
            .or_default()
            .push((user_id, chrono::offset::Local::now()))
    }

    fn add_site_to_ignore(&mut self, content: &str) -> Option<String> {
        let urls = self.extract_urls(content);
        let site = urls
            .first()
            .and_then(|u| u.host_str().map(|h| h.to_string()))?;

        if self.ignore_urls.contains(&site) {
            return None;
        }

        if self.always_enabled_urls.contains(&site) {
            Some(format!("Sorry, det går inte att stänga av {site}"))
        } else {
            self.ignore_urls.insert(site.clone());
            if let Err(err) = self.checkpoint() {
                log::error!("Failed to checkpoint: {err}");
            }

            Some(format!("Ok, ska sluta rapportera från {site}"))
        }
    }

    fn stats(&self) -> String {
        #[derive(Default)]
        struct Posts {
            total: u64,
            reposts: u64,
        }

        let mut stats: HashMap<UserId, Posts> = HashMap::new();
        let mut total_urls = 0u64;

        for posts in self.reposts.values() {
            total_urls += 1;

            for (i, post) in posts.iter().enumerate() {
                let entry = stats.entry(post.0).or_default();
                entry.total += 1;
                entry.reposts += if i > 0 { 1 } else { 0 };
            }
        }

        let mut s = String::new();
        s.push_str(format!("Totalt har det postats {total_urls} unika länkar\n").as_str());

        for (user_id, posts) in stats {
            let repost_percentage = if posts.reposts == 0 {
                0f64
            } else {
                (posts.reposts as f64 / posts.total as f64) * 100.
            };
            s.push_str(
                format!(
                    "- {}: {} ({} reposts, {}%)\n",
                    user_id.mention(),
                    posts.total,
                    posts.reposts,
                    repost_percentage as i16,
                )
                .as_str(),
            );
        }

        s
    }
}

/// The [`Handler`] implements the Discord trait to listen for messages and reactions.
struct Handler {
    repost_checker: Arc<Mutex<RepostChecker>>,
}

#[async_trait]
impl EventHandler for Handler {
    /// The message hook is fired on every message in the channel.
    async fn message(&self, ctx: Context, message: Message) {
        if message.author.id == ctx.cache.current_user().id {
            log::debug!("Skipping message from bot");
            return;
        }

        let messages = self.get_messages_to_send(&ctx, &message);

        for message_text in messages {
            let msg = CreateMessage::new()
                .embeds(Default::default())
                .content(message_text);

            if let Err(err) = message.channel_id.send_message(&ctx, msg).await {
                log::error!("Failed to send message: {err:?}");
            }
        }
    }

    /// Check reactions to messages.
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
                let mut rc = self.repost_checker.lock().unwrap();
                rc.add_site_to_ignore(&message.content)
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
    fn get_messages_to_send(&self, ctx: &Context, message: &Message) -> Vec<String> {
        let mut rc = self.repost_checker.lock().unwrap();

        let bot = ctx.cache().unwrap().current_user();
        if message.mentions.contains(&bot) && message.content.ends_with("stats") {
            return vec![rc.stats()];
        }

        let mut messages = Vec::new();

        let urls = rc.extract_urls(&message.content);
        if urls.is_empty() {
            return messages;
        }

        for u in urls {
            if let Some(repost) = rc.check_repost(&u, message.channel_id) {
                messages.push(repost);
            }

            rc.add_url(&u, message.author.id, message.channel_id);
        }

        if let Err(err) = rc.checkpoint() {
            log::error!("Failed to checkpoint: {err}");
        }

        messages
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("discord_repost"))
        .init();

    let config: DiscordConfig = serde_yaml::from_str(
        &std::fs::read_to_string("discord.yaml").expect("Failed to read config"),
    )
    .expect("Invalid config");
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let repost_checker = Arc::new(Mutex::new(
        RepostChecker::load(config.always_enabled_urls).unwrap(),
    ));
    let mut client = Client::builder(config.token, intents)
        .event_handler(Handler { repost_checker })
        .await
        .expect("Error creating client");

    log::info!("Starting bot...");

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
        let rc = RepostChecker::new(Default::default());
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
        let mut rc = RepostChecker::new(Default::default());

        let u = url::Url::parse("https://svt.se").unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).is_none());

        rc.add_url(&u, 123.into(), 1u64.into());
        assert!(rc.check_repost(&u, 1u64.into()).is_some());
    }

    #[test]
    fn test_stats() {
        let mut rc = RepostChecker::new(Default::default());

        let u = url::Url::parse("https://svt.se").unwrap();
        rc.add_url(&u, 123.into(), 1u64.into());

        assert!(rc.stats().contains("Totalt har det postats 1 unik"));
    }
}

