use serde::Deserialize;
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DATABASE_URL: &str = "sqlite:repost_checker.db";

/// [`DiscordConfig`] represents all the configuration required for Discord.
#[derive(Debug, Clone, Deserialize)]
struct DiscordConfig {
    token: String,
    always_enabled_urls: HashSet<String>,
}

/// [`RepostChecker`] is the storage of posted URLs, by whom and when it was posted. It's used to
/// check for already posted links.
struct RepostChecker {
    pool: SqlitePool,
    regex_pattern: regex::Regex,
    always_enabled_urls: HashSet<String>,
}

/// The regex used to find URLs. It only cares about anything that starts with the `http` or
/// `https` protocol.
fn default_regex() -> regex::Regex {
    regex::Regex::new(r"https?://\S+").expect("Invalid regex")
}

impl RepostChecker {
    /// Create a new [`RepostChecker`] with database connection
    async fn new(always_enabled_urls: HashSet<String>) -> Result<Self, sqlx::Error> {
        Self::new_with_url(DATABASE_URL, always_enabled_urls).await
    }

    /// Create a new [`RepostChecker`] with custom database URL (useful for testing)
    async fn new_with_url(database_url: &str, always_enabled_urls: HashSet<String>) -> Result<Self, sqlx::Error> {
        let pool = SqlitePool::connect(database_url).await?;
        
        // Create tables if they don't exist
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS reposts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL,
                user_id TEXT NOT NULL,
                posted_at TEXT NOT NULL
            );
            
            CREATE TABLE IF NOT EXISTS ignore_urls (
                domain TEXT PRIMARY KEY
            );
            
            CREATE TABLE IF NOT EXISTS always_enabled_urls (
                domain TEXT PRIMARY KEY
            );
            
            CREATE INDEX IF NOT EXISTS idx_reposts_url ON reposts(url);
            "#
        )
        .execute(&pool)
        .await?;
        
        // Insert always_enabled_urls into database
        for domain in &always_enabled_urls {
            sqlx::query("INSERT OR IGNORE INTO always_enabled_urls (domain) VALUES (?)")
                .bind(domain)
                .execute(&pool)
                .await?;
        }
        
        Ok(Self {
            pool,
            regex_pattern: default_regex(),
            always_enabled_urls,
        })
    }

    /// Load existing state and setup database
    async fn load(always_enabled_urls: HashSet<String>) -> Result<Self, sqlx::Error> {
        Self::new(always_enabled_urls).await
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
    async fn check_repost(&self, u: &url::Url, _channel_id: ChannelId) -> Option<String> {
        let url_str = u.to_string();
        let host = u.host_str().unwrap_or_default();

        // Check if this domain is ignored
        let ignored = sqlx::query("SELECT 1 FROM ignore_urls WHERE domain = ?")
            .bind(host)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None);

        if ignored.is_some() {
            return None;
        }

        // Get all reposts for this URL
        let reposts = sqlx::query("SELECT user_id, posted_at FROM reposts WHERE url = ? ORDER BY posted_at")
            .bind(&url_str)
            .fetch_all(&self.pool)
            .await
            .ok()?;

        if reposts.is_empty() {
            return None;
        }

        let seen_count = reposts.len();
        let times_text = if seen_count == 1 { "gång" } else { "gånger" };
        let repost_list = reposts
            .iter()
            .map(|row| {
                let user_id: String = row.get("user_id");
                let posted_at: String = row.get("posted_at");
                let uid = UserId::from(user_id.parse::<u64>().unwrap_or(0)).mention();
                format!("- {uid} - {posted_at}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        Some(format!(
            "{url_str} har postats {} {}!\n{}",
            seen_count, times_text, repost_list
        ))
    }

    /// Add a URL as seen by the passed `user_id`.
    async fn add_url(&self, u: &url::Url, user_id: UserId, _channel_id: ChannelId) -> Result<(), sqlx::Error> {
        let url_str = u.to_string();
        let user_id_str = user_id.to_string();
        let now = chrono::offset::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        
        sqlx::query("INSERT INTO reposts (url, user_id, posted_at) VALUES (?, ?, ?)")
            .bind(&url_str)
            .bind(&user_id_str)
            .bind(&now)
            .execute(&self.pool)
            .await?;
            
        Ok(())
    }

    async fn add_site_to_ignore(&self, content: &str) -> Option<String> {
        let urls = self.extract_urls(content);
        let site = urls
            .first()
            .and_then(|u| u.host_str().map(|h| h.to_string()))?;

        // Check if already ignored
        let already_ignored = sqlx::query("SELECT 1 FROM ignore_urls WHERE domain = ?")
            .bind(&site)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None)
            .is_some();

        if already_ignored {
            return None;
        }

        if self.always_enabled_urls.contains(&site) {
            Some(format!("Sorry, det går inte att stänga av {site}"))
        } else {
            if let Err(err) = sqlx::query("INSERT INTO ignore_urls (domain) VALUES (?)")
                .bind(&site)
                .execute(&self.pool)
                .await 
            {
                log::error!("Failed to add site to ignore list: {err}");
                return None;
            }

            Some(format!("Ok, ska sluta rapportera från {site}"))
        }
    }

    async fn stats(&self) -> String {
        #[derive(Default)]
        struct Posts {
            total: u64,
            reposts: u64,
        }

        let mut stats: HashMap<UserId, Posts> = HashMap::new();
        
        // Get total unique URLs
        let total_urls: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT url) FROM reposts")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        // Get all reposts ordered by URL and then by posted_at
        let all_reposts = sqlx::query("SELECT url, user_id FROM reposts ORDER BY url, posted_at")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut current_url = String::new();
        let mut url_post_count = 0;
        
        for row in all_reposts {
            let url: String = row.get("url");
            let user_id_str: String = row.get("user_id");
            let user_id = UserId::from(user_id_str.parse::<u64>().unwrap_or(0));
            
            if url != current_url {
                current_url = url;
                url_post_count = 0;
            }
            
            let entry = stats.entry(user_id).or_default();
            entry.total += 1;
            entry.reposts += if url_post_count > 0 { 1 } else { 0 };
            
            url_post_count += 1;
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
    repost_checker: Arc<RepostChecker>,
}

#[async_trait]
impl EventHandler for Handler {
    /// The message hook is fired on every message in the channel.
    async fn message(&self, ctx: Context, message: Message) {
        if message.author.id == ctx.cache.current_user().id {
            log::debug!("Skipping message from bot");
            return;
        }

        let messages = self.get_messages_to_send(&ctx, &message).await;

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
                self.repost_checker.add_site_to_ignore(&message.content).await
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
    async fn get_messages_to_send(&self, ctx: &Context, message: &Message) -> Vec<String> {
        let bot_id = ctx.cache().unwrap().current_user().id;
        if message.mentions.iter().any(|u| u.id == bot_id) && message.content.ends_with("stats") {
            return vec![self.repost_checker.stats().await];
        }

        let mut messages = Vec::new();

        let urls = self.repost_checker.extract_urls(&message.content);
        if urls.is_empty() {
            return messages;
        }

        for u in urls {
            if let Some(repost) = self.repost_checker.check_repost(&u, message.channel_id).await {
                messages.push(repost);
            }

            if let Err(err) = self.repost_checker.add_url(&u, message.author.id, message.channel_id).await {
                log::error!("Failed to add URL: {err}");
            }
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
    let repost_checker = Arc::new(
        RepostChecker::load(config.always_enabled_urls).await.expect("Failed to initialize database"),
    );
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

    #[tokio::test]
    async fn test_parse_url() {
        let rc = RepostChecker::new_with_url("sqlite::memory:", Default::default()).await.unwrap();
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

    #[tokio::test]
    async fn test_check_repost() {
        let rc = RepostChecker::new_with_url("sqlite::memory:", Default::default()).await.unwrap();

        let u = url::Url::parse("https://svt.se").unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).await.is_none());

        rc.add_url(&u, 123.into(), 1u64.into()).await.unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).await.is_some());
    }

    #[tokio::test]
    async fn test_stats() {
        let rc = RepostChecker::new_with_url("sqlite::memory:", Default::default()).await.unwrap();

        let u = url::Url::parse("https://svt.se").unwrap();
        rc.add_url(&u, 123.into(), 1u64.into()).await.unwrap();

        assert!(rc.stats().await.contains("Totalt har det postats 1 unik"));
    }
}

