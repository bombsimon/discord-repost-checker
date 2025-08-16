use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// The path to the SQLite database.
const DATABASE_URL: &str = "sqlite:repost_checker.db";

/// The regex used to find URLs. It only cares about anything that starts with the `http` or
/// `https` protocol.
static URL_REGEX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"https?://\S+").expect("Invalid regex"));

/// [`RepostChecker`] is the storage of posted URLs, by whom and when it was posted. It's used to
/// check for already posted links.
struct RepostChecker {
    pool: SqlitePool,
}

impl RepostChecker {
    /// Create a new [`RepostChecker`] with database connection
    async fn new() -> Result<Self, sqlx::Error> {
        Self::new_with_url(DATABASE_URL).await
    }

    /// Create a new [`RepostChecker`] with custom database URL (useful for testing)
    async fn new_with_url(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = if database_url == "sqlite::memory:" {
            SqlitePool::connect(database_url).await?
        } else {
            let options = SqliteConnectOptions::new()
                .filename(database_url.strip_prefix("sqlite:").unwrap_or(database_url))
                .create_if_missing(true);

            SqlitePool::connect_with(options).await?
        };

        // Create tables if they don't exist
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS reposts (
                url TEXT NOT NULL,
                user_id TEXT NOT NULL,
                posted_at TEXT NOT NULL
            );
            
            CREATE TABLE IF NOT EXISTS ignore_hosts (
                host TEXT PRIMARY KEY
            );
            
            CREATE TABLE IF NOT EXISTS always_enabled_hosts (
                host TEXT PRIMARY KEY
            );
            
            CREATE INDEX IF NOT EXISTS idx_reposts_url ON reposts(url);
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    /// Extract all URLs from a test string. Used to check if a Discord message contains any URLs
    /// that we need to check for repost.
    fn extract_urls(&self, source: &str) -> Vec<url::Url> {
        URL_REGEX
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
        let ignored = sqlx::query("SELECT 1 FROM ignore_hosts WHERE host = ?")
            .bind(host)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None);

        if ignored.is_some() {
            return None;
        }

        // Get all reposts for this URL
        let reposts =
            sqlx::query("SELECT user_id, posted_at FROM reposts WHERE url = ? ORDER BY posted_at")
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
            "{url_str} har postats {seen_count} {times_text}!\n{repost_list}",
        ))
    }

    /// Add a URL as seen by the passed `user_id`.
    async fn add_url(
        &self,
        u: &url::Url,
        user_id: UserId,
        _channel_id: ChannelId,
    ) -> Result<(), sqlx::Error> {
        let url_str = u.to_string();
        let user_id_str = user_id.to_string();
        let now = chrono::offset::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

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
        let already_ignored = sqlx::query("SELECT 1 FROM ignore_hosts WHERE host = ?")
            .bind(&site)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None)
            .is_some();

        if already_ignored {
            return None;
        }

        // Check if this domain is in always_enabled_hosts
        let always_enabled = sqlx::query("SELECT 1 FROM always_enabled_hosts WHERE host = ?")
            .bind(&site)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None);

        if always_enabled.is_some() {
            Some(format!("Sorry, det går inte att stänga av {site}"))
        } else {
            if let Err(err) = self.insert_ignore_host(&site).await {
                log::error!("Failed to add site to ignore list: {err}");
                return None;
            }

            Some(format!("Ok, ska sluta rapportera från {site}"))
        }
    }

    async fn admin_add_always_enabled(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO always_enabled_hosts (host) VALUES (?)")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Added {host} to always enabled hosts"))
    }

    async fn admin_remove_always_enabled(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("DELETE FROM always_enabled_hosts WHERE host = ?")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Removed {host} from always enabled hosts"))
    }

    async fn insert_ignore_host(&self, host: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO ignore_hosts (host) VALUES (?)")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn admin_add_ignore(&self, host: &str) -> Result<String, sqlx::Error> {
        self.insert_ignore_host(host).await?;
        Ok(format!("Added {host} to ignored hosts"))
    }

    async fn admin_remove_ignore(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("DELETE FROM ignore_hosts WHERE host = ?")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Removed {host} from ignored hosts"))
    }

    async fn admin_list_urls(&self) -> Result<String, sqlx::Error> {
        let always_enabled: Vec<String> =
            sqlx::query_scalar("SELECT host FROM always_enabled_hosts ORDER BY host")
                .fetch_all(&self.pool)
                .await?;

        let ignored: Vec<String> =
            sqlx::query_scalar("SELECT host FROM ignore_hosts ORDER BY host")
                .fetch_all(&self.pool)
                .await?;

        let mut result = String::new();

        result.push_str("**Always Enabled Hosts:**\n");

        if always_enabled.is_empty() {
            result.push_str("(none)\n");
        } else {
            for domain in always_enabled {
                result.push_str(&format!("- {}\n", domain));
            }
        }

        result.push_str("\n**Ignored Hosts:**\n");
        if ignored.is_empty() {
            result.push_str("(none)\n");
        } else {
            for domain in ignored {
                result.push_str(&format!("- {}\n", domain));
            }
        }

        Ok(result)
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

        // Handle DM admin commands
        if message.guild_id.is_none() {
            if let Some(response) = self.handle_admin_command(&ctx, &message).await {
                let msg = CreateMessage::new().content(response);
                if let Err(err) = message.channel_id.send_message(&ctx, msg).await {
                    log::error!("Failed to send DM response: {err:?}");
                }
            }

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
                self.repost_checker
                    .add_site_to_ignore(&message.content)
                    .await
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
            if let Some(repost) = self
                .repost_checker
                .check_repost(&u, message.channel_id)
                .await
            {
                messages.push(repost);
            }

            if let Err(err) = self
                .repost_checker
                .add_url(&u, message.author.id, message.channel_id)
                .await
            {
                log::error!("Failed to add URL: {err}");
            }
        }

        messages
    }

    async fn handle_admin_command(&self, ctx: &Context, message: &Message) -> Option<String> {
        // Check if user is admin in any mutual guild
        if !self.is_user_admin(ctx, message.author.id).await {
            return Some("You need administrator permissions to use admin commands.".to_string());
        }

        let content = message.content.trim();
        let parts: Vec<&str> = content.split_whitespace().collect();

        let command = parts[0];

        if command == "list-urls" {
            return match self.repost_checker.admin_list_urls().await {
                Ok(msg) => Some(msg),
                Err(e) => Some(format!("Error: {e}")),
            };
        }

        if parts.len() != 2 {
            return Some(
                r#"
Available commands:
- `always-enable <host>` - Always check reposts for host
- `always-disable <host>` - Remove host from always enabled
- `ignore-add <host>` - Ignore reposts from host
- `ignore-remove <host>` - Remove host from ignore list
- `list-urls` - List all always-enabled and ignored hosts
"#
                .to_string(),
            );
        }

        let host = parts[1];

        match command {
            "always-enable-add" => {
                match self.repost_checker.admin_add_always_enabled(host).await {
                    Ok(msg) => Some(msg),
                    Err(e) => Some(format!("Error: {e}")),
                }
            }
            "always-disable-remove" => {
                match self.repost_checker.admin_remove_always_enabled(host).await {
                    Ok(msg) => Some(msg),
                    Err(e) => Some(format!("Error: {e}")),
                }
            }
            "ignore-add" => {
                match self.repost_checker.admin_add_ignore(host).await {
                    Ok(msg) => Some(msg),
                    Err(e) => Some(format!("Error: {e}")),
                }
            }
            "ignore-remove" => {
                match self.repost_checker.admin_remove_ignore(host).await {
                    Ok(msg) => Some(msg),
                    Err(e) => Some(format!("Error: {e}")),
                }
            }
            _ => Some("Unknown command. Available: always-enable-add, always-enable-remove, ignore-add, ignore-remove".to_string()),
        }
    }

    async fn is_user_admin(&self, ctx: &Context, user_id: UserId) -> bool {
        // Check all guilds where the bot and user are both present
        for guild_id in ctx.cache.guilds() {
            if let Ok(member) = guild_id.member(ctx, user_id).await {
                if let Ok(guild) = guild_id.to_partial_guild(ctx).await {
                    // This is in fact no longer deprecated: https://github.com/serenity-rs/serenity/pull/3314
                    #[allow(deprecated)]
                    let permissions = guild.member_permissions(&member);
                    if permissions.administrator() {
                        return true;
                    }
                }
            }
        }

        false
    }
}

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

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_parse_url() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();
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
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        let u = url::Url::parse("https://svt.se").unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).await.is_none());

        rc.add_url(&u, 123.into(), 1u64.into()).await.unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).await.is_some());
    }

    #[tokio::test]
    async fn test_stats() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        let u = url::Url::parse("https://svt.se").unwrap();
        rc.add_url(&u, 123.into(), 1u64.into()).await.unwrap();

        assert!(rc.stats().await.contains("Totalt har det postats 1 unik"));
    }

    #[tokio::test]
    async fn test_admin_list_urls() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Test empty lists
        let result = rc.admin_list_urls().await.unwrap();
        assert!(result.contains("**Always Enabled Hosts:**"));
        assert!(result.contains("**Ignored Hosts:**"));
        assert!(result.contains("(none)"));

        // Add some domains
        rc.admin_add_always_enabled("example.com").await.unwrap();
        rc.admin_add_always_enabled("test.org").await.unwrap();
        rc.admin_add_ignore("spam.net").await.unwrap();

        let result = rc.admin_list_urls().await.unwrap();

        // Check that domains are listed and sorted
        assert!(result.contains("- example.com"));
        assert!(result.contains("- test.org"));
        assert!(result.contains("- spam.net"));

        // Verify sorting (example.com should come before test.org)
        let example_pos = result.find("- example.com").unwrap();
        let test_pos = result.find("- test.org").unwrap();
        assert!(example_pos < test_pos);
    }
}
