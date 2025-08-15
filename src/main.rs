use serde::Deserialize;
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const DB_FILE: &str = "reposts.db";

/// [`DiscordConfig`] represents all the configuration required for Discord.
#[derive(Debug, Clone, Deserialize)]
struct DiscordConfig {
    token: String,
    always_enabled_urls: HashSet<String>,
}

struct Database {
    conn: rusqlite::Connection,
}

impl Database {
    fn new() -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open(DB_FILE)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS reposts (
                url TEXT NOT NULL,
                user_id INTEGER NOT NULL,
                posted_at TEXT NOT NULL,
                original_poster INTEGER NOT NULL
            )",
            (),
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS ignored_urls (
                url TEXT NOT NULL UNIQUE
            )",
            (),
        )?;
        Ok(Self { conn })
    }

    fn add_url(&self, u: &url::Url, user_id: UserId) -> Result<(), rusqlite::Error> {
        let original_poster: u64 = {
            let mut stmt = self
                .conn
                .prepare("SELECT original_poster FROM reposts WHERE url = ?1 LIMIT 1")?;
            let mut rows = stmt.query_map(rusqlite::params![u.to_string()], |row| row.get(0))?;
            if let Some(Ok(id)) = rows.next() {
                id
            } else {
                user_id.get()
            }
        };

        self.conn.execute(
            "INSERT INTO reposts (url, user_id, posted_at, original_poster) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                u.to_string(),
                user_id.get(),
                chrono::Local::now().to_rfc3339(),
                original_poster,
            ],
        )?;
        Ok(())
    }

    fn check_repost(
        &self,
        u: &url::Url,
    ) -> Result<Vec<(UserId, chrono::DateTime<chrono::Local>)>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT user_id, posted_at FROM reposts WHERE url = ?1")?;
        let rows = stmt.query_map(rusqlite::params![u.to_string()], |row| {
            let user_id: u64 = row.get(0)?;
            let posted_at: String = row.get(1)?;
            Ok((
                UserId::from(user_id),
                chrono::DateTime::parse_from_rfc3339(&posted_at)
                    .unwrap()
                    .with_timezone(&chrono::Local),
            ))
        })?;

        let mut reposts = Vec::new();
        for row in rows {
            reposts.push(row?);
        }
        Ok(reposts)
    }

    fn add_site_to_ignore(&self, site: &str) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("INSERT INTO ignored_urls (url) VALUES (?1)", [site])?;
        Ok(())
    }

    fn is_site_ignored(&self, site: &str) -> Result<bool, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM ignored_urls WHERE url = ?1")?;
        stmt.exists([site])
    }

    fn stats(&self) -> Result<String, rusqlite::Error> {
        #[derive(Default)]
        struct Posts {
            total: u64,
            reposts: u64,
        }

        let mut stats: HashMap<UserId, Posts> = HashMap::new();
        let mut unique_urls = HashSet::new();

        let mut stmt = self
            .conn
            .prepare("SELECT url, user_id, original_poster FROM reposts")?;
        let rows = stmt.query_map([], |row| {
            let url: String = row.get(0)?;
            let user_id: u64 = row.get(1)?;
            let original_poster: u64 = row.get(2)?;
            Ok((url, UserId::from(user_id), UserId::from(original_poster)))
        })?;

        for row in rows {
            let (url, user_id, original_poster) = row?;
            unique_urls.insert(url);
            let entry = stats.entry(user_id).or_default();
            entry.total += 1;

            if user_id != original_poster {
                entry.reposts += 1;
            }
        }

        let mut s = String::new();
        s.push_str(
            format!(
                "Totalt har det postats {} unika länkar\n",
                unique_urls.len()
            )
            .as_str(),
        );

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

        Ok(s)
    }
}

/// [`RepostChecker`] is the storage of posted URLs, by whom and when it was posted. It's used to
/// check for already posted links.
struct RepostChecker {
    db: Arc<Mutex<Database>>,
    regex_pattern: regex::Regex,
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
            db: Arc::new(Mutex::new(Database::new().unwrap())),
            regex_pattern: default_regex(),
            always_enabled_urls,
        }
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
        let host = u.host_str().unwrap_or_default();
        if self
            .db
            .lock()
            .unwrap()
            .is_site_ignored(host)
            .unwrap_or(false)
        {
            return None;
        }

        match self.db.lock().unwrap().check_repost(u) {
            Ok(repost) if !repost.is_empty() => {
                let seen_count = repost.len();
                let times_text = if seen_count == 1 { "gång" } else { "gånger" };
                let repost_list = repost
                    .iter()
                    .map(|(user_id, when)| {
                        let uid = UserId::from(*user_id).mention();
                        format!("- {uid} - {}", when.format("%Y-%m-%d %H:%M:%S"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Some(format!(
                    "{url_str} har postats {seen_count} {times_text}!\n{repost_list}",
                    url_str = u.to_string(),
                ))
            }
            _ => None,
        }
    }

    /// Add a URL as seen by the passed `user_id`.
    fn add_url(&mut self, u: &url::Url, user_id: UserId, _channel_id: ChannelId) {
        if let Err(err) = self.db.lock().unwrap().add_url(u, user_id) {
            log::error!("Failed to add URL to database: {err}");
        }
    }

    fn add_site_to_ignore(&mut self, content: &str) -> Option<String> {
        let urls = self.extract_urls(content);
        let site = urls
            .first()
            .and_then(|u| u.host_str().map(|h| h.to_string()))?;

        if self
            .db
            .lock()
            .unwrap()
            .is_site_ignored(&site)
            .unwrap_or(false)
        {
            return None;
        }

        if self.always_enabled_urls.contains(&site) {
            Some(format!("Sorry, det går inte att stänga av {site}"))
        } else {
            if let Err(err) = self.db.lock().unwrap().add_site_to_ignore(&site) {
                log::error!("Failed to add site to ignore list: {err}");
                return Some(format!("Misslyckades att ignorera {site}"));
            }

            Some(format!("Ok, ska sluta rapportera från {site}"))
        }
    }

    fn stats(&self) -> String {
        match self.db.lock().unwrap().stats() {
            Ok(stats) => stats,
            Err(err) => {
                log::error!("Failed to get stats: {err}");
                "Misslyckades att hämta statistik".to_string()
            }
        }
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
    let repost_checker = Arc::new(Mutex::new(RepostChecker::new(config.always_enabled_urls)));
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

    fn new_in_memory_db() -> Arc<Mutex<Database>> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS reposts (
                url TEXT NOT NULL,
                user_id INTEGER NOT NULL,
                posted_at TEXT NOT NULL,
                original_poster INTEGER NOT NULL
            )",
            (),
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS ignored_urls (
                url TEXT NOT NULL UNIQUE
            )",
            (),
        )
        .unwrap();
        Arc::new(Mutex::new(Database { conn }))
    }

    fn new_repost_checker(db: Arc<Mutex<Database>>) -> RepostChecker {
        RepostChecker {
            db,
            regex_pattern: default_regex(),
            always_enabled_urls: Default::default(),
        }
    }

    #[test]
    fn test_parse_url() {
        let db = new_in_memory_db();
        let rc = new_repost_checker(db.clone());
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
        let db = new_in_memory_db();
        let mut rc = new_repost_checker(db.clone());

        let u = url::Url::parse("https://svt.se").unwrap();
        assert!(rc.check_repost(&u, 1u64.into()).is_none());

        rc.add_url(&u, 123.into(), 1u64.into());
        assert!(rc.check_repost(&u, 1u64.into()).is_some());
    }

    #[test]
    fn test_stats() {
        let db = new_in_memory_db();
        let mut rc = new_repost_checker(db.clone());

        let u = url::Url::parse("https://svt.se").unwrap();
        rc.add_url(&u, 123.into(), 1u64.into());

        assert!(rc.stats().contains("Totalt har det postats 1 unika länkar"));
    }

    #[test]
    fn test_ignore_site() {
        let db = new_in_memory_db();
        let mut rc = new_repost_checker(db.clone());

        let u = url::Url::parse("https://svt.se").unwrap();
        rc.add_site_to_ignore("svt.se");

        assert!(rc.check_repost(&u, 1u64.into()).is_none());
    }

    #[test]
    fn test_stats_multiple_users() {
        let db = new_in_memory_db();
        let mut rc = new_repost_checker(db.clone());

        let u1 = url::Url::parse("https://svt.se").unwrap();
        let u2 = url::Url::parse("https://dn.se").unwrap();

        rc.add_url(&u1, 123.into(), 1u64.into());
        rc.add_url(&u1, 456.into(), 1u64.into());
        rc.add_url(&u2, 123.into(), 1u64.into());

        let stats = rc.stats();
        assert!(stats.contains("Totalt har det postats 2 unika länkar"));
        assert!(stats.contains("- <@123>: 2 (0 reposts, 0%)"));
        assert!(stats.contains("- <@456>: 1 (1 reposts, 100%)"));
    }
}
