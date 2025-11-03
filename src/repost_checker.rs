use serenity::model::prelude::*;
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::sync::LazyLock;

/// The regex used to find URLs. It only cares about anything that starts with the `http` or
/// `https` protocol.
static URL_REGEX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"https?://\S+").expect("Invalid regex"));

/// [`RepostChecker`] is the storage of posted URLs, by whom and when it was posted. It's used to
/// check for already posted links.
pub struct RepostChecker {
    pool: SqlitePool,
}

impl RepostChecker {
    /// Create a new [`RepostChecker`] with database connection
    pub async fn new() -> Result<Self, sqlx::Error> {
        Self::new_with_url("sqlite:repost_checker.db").await
    }

    /// Create a new [`RepostChecker`] with custom database URL (useful for testing)
    pub async fn new_with_url(database_url: &str) -> Result<Self, sqlx::Error> {
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

            CREATE TABLE IF NOT EXISTS preserve_full_url_hosts (
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
    pub fn extract_urls(&self, source: &str) -> Vec<url::Url> {
        URL_REGEX
            .find_iter(source)
            .filter_map(|u| url::Url::parse(u.as_str()).ok())
            .collect()
    }

    /// Sanitize a URL by removing query strings and fragments
    fn sanitize_url(u: &url::Url) -> String {
        let mut sanitized = u.clone();
        sanitized.set_query(None);
        sanitized.set_fragment(None);
        sanitized.to_string()
    }

    /// Check if the passed URL has been posted before. If so, return a message saying how many
    /// times and by which user it's been posted.
    pub async fn check_repost(&self, u: &url::Url, _channel_id: ChannelId) -> Option<String> {
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

        // Check if this host requires exact URL matching
        let preserve_full_url = sqlx::query("SELECT 1 FROM preserve_full_url_hosts WHERE host = ?")
            .bind(host)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None)
            .is_some();

        // Get all reposts for this URL
        let reposts = if preserve_full_url {
            // Use exact match for hosts that need full URL matching
            sqlx::query("SELECT user_id, posted_at FROM reposts WHERE url = ? ORDER BY posted_at")
                .bind(&url_str)
                .fetch_all(&self.pool)
                .await
                .ok()?
        } else {
            // Use LIKE query to match URLs ignoring query strings and fragments
            let sanitized = Self::sanitize_url(u);
            sqlx::query(
                "SELECT user_id, posted_at FROM reposts WHERE url LIKE ? ORDER BY posted_at",
            )
            .bind(format!("{sanitized}%"))
            .fetch_all(&self.pool)
            .await
            .ok()?
        };

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
    pub async fn add_url(
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

    pub async fn add_site_to_ignore(&self, content: &str) -> Option<String> {
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

    pub async fn add_site_to_preserve_full_url(&self, content: &str) -> Option<String> {
        let urls = self.extract_urls(content);
        let site = urls
            .first()
            .and_then(|u| u.host_str().map(|h| h.to_string()))?;

        // Check if already in preserve list
        let already_preserved = sqlx::query("SELECT 1 FROM preserve_full_url_hosts WHERE host = ?")
            .bind(&site)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None)
            .is_some();

        if already_preserved {
            return None;
        }

        if let Err(err) = self.insert_preserve_full_url_host(&site).await {
            log::error!("Failed to add site to preserve full URL list: {err}");
            return None;
        }

        Some(format!("Ok, ska matcha fullständiga URLer från {site}"))
    }

    pub async fn admin_add_always_enabled(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO always_enabled_hosts (host) VALUES (?)")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Added {host} to always enabled hosts"))
    }

    pub async fn admin_remove_always_enabled(&self, host: &str) -> Result<String, sqlx::Error> {
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

    async fn insert_preserve_full_url_host(&self, host: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO preserve_full_url_hosts (host) VALUES (?)")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn admin_add_ignore(&self, host: &str) -> Result<String, sqlx::Error> {
        self.insert_ignore_host(host).await?;
        Ok(format!("Added {host} to ignored hosts"))
    }

    pub async fn admin_remove_ignore(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("DELETE FROM ignore_hosts WHERE host = ?")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Removed {host} from ignored hosts"))
    }

    pub async fn admin_add_preserve_full_url(&self, host: &str) -> Result<String, sqlx::Error> {
        self.insert_preserve_full_url_host(host).await?;
        Ok(format!("Added {host} to preserve full URL hosts"))
    }

    pub async fn admin_remove_preserve_full_url(&self, host: &str) -> Result<String, sqlx::Error> {
        sqlx::query("DELETE FROM preserve_full_url_hosts WHERE host = ?")
            .bind(host)
            .execute(&self.pool)
            .await?;

        Ok(format!("Removed {host} from preserve full URL hosts"))
    }

    pub async fn admin_list_urls(&self) -> Result<String, sqlx::Error> {
        let always_enabled: Vec<String> =
            sqlx::query_scalar("SELECT host FROM always_enabled_hosts ORDER BY host")
                .fetch_all(&self.pool)
                .await?;

        let ignored: Vec<String> =
            sqlx::query_scalar("SELECT host FROM ignore_hosts ORDER BY host")
                .fetch_all(&self.pool)
                .await?;

        let preserve_full_url: Vec<String> =
            sqlx::query_scalar("SELECT host FROM preserve_full_url_hosts ORDER BY host")
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

        result.push_str("\n**Preserve Full URL Hosts:**\n");
        if preserve_full_url.is_empty() {
            result.push_str("(none)\n");
        } else {
            for domain in preserve_full_url {
                result.push_str(&format!("- {}\n", domain));
            }
        }

        Ok(result)
    }

    pub async fn stats(&self) -> String {
        // Get total unique URLs
        let total_urls: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT url) FROM reposts")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        // Get user stats using SQL aggregation
        let user_stats = sqlx::query(
            r#"
            WITH user_posts AS (
                SELECT 
                    user_id,
                    url,
                    posted_at,
                    ROW_NUMBER() OVER (PARTITION BY url ORDER BY posted_at) as post_order
                FROM reposts
            ),
            user_aggregates AS (
                SELECT 
                    user_id,
                    COUNT(*) as total_posts,
                    COUNT(CASE WHEN post_order > 1 THEN 1 END) as repost_count
                FROM user_posts
                GROUP BY user_id
            )
            SELECT 
                user_id,
                total_posts,
                repost_count,
                CASE 
                    WHEN total_posts = 0 THEN 0
                    ELSE ROUND((repost_count * 100.0) / total_posts)
                END as repost_percentage
            FROM user_aggregates
            ORDER BY total_posts DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut s = String::new();
        s.push_str(&format!(
            "Totalt har det postats {total_urls} unika länkar\n"
        ));

        for row in user_stats {
            let user_id_str: String = row.get("user_id");
            let user_id = UserId::from(user_id_str.parse::<u64>().unwrap_or(0));
            let total_posts: i64 = row.get("total_posts");
            let repost_count: i64 = row.get("repost_count");
            let repost_percentage: f64 = row.get("repost_percentage");

            s.push_str(&format!(
                "- {}: {} ({} reposts, {}%)\n",
                user_id.mention(),
                total_posts,
                repost_count,
                repost_percentage as i64,
            ));
        }

        s
    }

    pub async fn top_domains(&self) -> String {
        let domain_stats = sqlx::query(
            r#"
            WITH domain_posts AS (
                SELECT
                    CASE
                        WHEN url LIKE 'https://%' THEN SUBSTR(url, 9)
                        WHEN url LIKE 'http://%' THEN SUBSTR(url, 8)
                        ELSE url
                    END as domain_part
                FROM reposts
            ),
            domains AS (
                SELECT
                    CASE
                        WHEN INSTR(domain_part, '/') > 0
                        THEN SUBSTR(domain_part, 1, INSTR(domain_part, '/') - 1)
                        ELSE domain_part
                    END as domain,
                    COUNT(*) as post_count
                FROM domain_posts
                GROUP BY domain
            )
            SELECT domain, post_count
            FROM domains
            ORDER BY post_count DESC
            LIMIT 5
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut s = String::new();
        s.push_str("Top 5 domäner:\n");

        if domain_stats.is_empty() {
            s.push_str("Inga domäner hittades\n");
        } else {
            for (i, row) in domain_stats.iter().enumerate() {
                let domain: String = row.get("domain");
                let count: i64 = row.get("post_count");
                s.push_str(&format!("{}. {} ({} länkar)\n", i + 1, domain, count));
            }
        }

        s
    }

    pub async fn top_users(&self) -> String {
        let user_stats = sqlx::query(
            r#"
            SELECT user_id, COUNT(*) as post_count
            FROM reposts
            GROUP BY user_id
            ORDER BY post_count DESC
            LIMIT 5
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut s = String::new();
        s.push_str("Top 5 användare:\n");

        if user_stats.is_empty() {
            s.push_str("Inga användare hittades\n");
        } else {
            for (i, row) in user_stats.iter().enumerate() {
                let user_id_str: String = row.get("user_id");
                let user_id = UserId::from(user_id_str.parse::<u64>().unwrap_or(0));
                let count: i64 = row.get("post_count");
                s.push_str(&format!(
                    "{}. {} ({} länkar)\n",
                    i + 1,
                    user_id.mention(),
                    count
                ));
            }
        }

        s
    }

    pub async fn today_stats(&self) -> String {
        let today = chrono::offset::Local::now().format("%Y-%m-%d").to_string();

        let today_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM reposts WHERE DATE(posted_at) = ?")
                .bind(&today)
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);

        format!("Idag har det postats {} länkar", today_count)
    }

    pub async fn top_domains_by_user(&self) -> String {
        let user_domains = sqlx::query(
            r#"
            WITH domain_posts AS (
                SELECT
                    user_id,
                    CASE
                        WHEN url LIKE 'https://%' THEN SUBSTR(url, 9)
                        WHEN url LIKE 'http://%' THEN SUBSTR(url, 8)
                        ELSE url
                    END as domain_part
                FROM reposts
            ),
            user_domains AS (
                SELECT
                    user_id,
                    CASE
                        WHEN INSTR(domain_part, '/') > 0
                        THEN SUBSTR(domain_part, 1, INSTR(domain_part, '/') - 1)
                        ELSE domain_part
                    END as domain,
                    COUNT(*) as post_count
                FROM domain_posts
                GROUP BY user_id, domain
            ),
            user_top_domains AS (
                SELECT
                    user_id,
                    domain,
                    post_count,
                    ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY post_count DESC) as rank
                FROM user_domains
            )
            SELECT user_id, domain, post_count
            FROM user_top_domains
            WHERE rank <= 5
            ORDER BY user_id, post_count DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        if user_domains.is_empty() {
            return "Inga domäner hittades".to_string();
        }

        let mut result = String::new();
        result.push_str("Top domäner per användare:\n");

        let mut current_user: Option<String> = None;
        let mut user_domains_list: Vec<String> = Vec::new();

        for row in user_domains {
            let user_id_str: String = row.get("user_id");
            let domain: String = row.get("domain");
            let count: i64 = row.get("post_count");

            if current_user.as_ref() != Some(&user_id_str) {
                if let Some(user) = current_user {
                    let user_id = UserId::from(user.parse::<u64>().unwrap_or(0));
                    result.push_str(&format!(
                        "- {}: {}\n",
                        user_id.mention(),
                        user_domains_list.join(", ")
                    ));
                }
                current_user = Some(user_id_str.clone());
                user_domains_list.clear();
            }

            user_domains_list.push(format!("{} ({})", domain, count));
        }

        if let Some(user) = current_user {
            let user_id = UserId::from(user.parse::<u64>().unwrap_or(0));
            result.push_str(&format!(
                "- {}: {}\n",
                user_id.mention(),
                user_domains_list.join(", ")
            ));
        }

        result
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

        let u1 = url::Url::parse("https://svt.se").unwrap();
        let u2 = url::Url::parse("https://aftonbladet.se").unwrap();

        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u1, 456.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u1, 789.into(), 1u64.into()).await.unwrap();

        rc.add_url(&u2, 456.into(), 1u64.into()).await.unwrap();

        let expected = r#"Totalt har det postats 2 unika länkar
- <@456>: 2 (1 reposts, 50%)
- <@123>: 1 (0 reposts, 0%)
- <@789>: 1 (1 reposts, 100%)
"#;

        assert_eq!(rc.stats().await, expected);
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

    #[tokio::test]
    async fn test_top_domains() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Test empty database
        let result = rc.top_domains().await;
        assert!(result.contains("Top 5 domäner:"));
        assert!(result.contains("Inga domäner hittades"));

        // Add some URLs
        let u1 = url::Url::parse("https://github.com/test").unwrap();
        let u2 = url::Url::parse("https://stackoverflow.com/questions/123").unwrap();
        let u3 = url::Url::parse("https://github.com/another").unwrap();

        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u2, 456.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u3, 789.into(), 1u64.into()).await.unwrap();

        let result = rc.top_domains().await;
        assert!(result.contains("1. github.com (2 länkar)"));
        assert!(result.contains("2. stackoverflow.com (1 länkar)"));
    }

    #[tokio::test]
    async fn test_top_users() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Test empty database
        let result = rc.top_users().await;
        assert!(result.contains("Top 5 användare:"));
        assert!(result.contains("Inga användare hittades"));

        // Add some URLs
        let u1 = url::Url::parse("https://example.com/1").unwrap();
        let u2 = url::Url::parse("https://example.com/2").unwrap();
        let u3 = url::Url::parse("https://example.com/3").unwrap();

        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u2, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u3, 456.into(), 1u64.into()).await.unwrap();

        let result = rc.top_users().await;
        assert!(result.contains("1. <@123> (2 länkar)"));
        assert!(result.contains("2. <@456> (1 länkar)"));
    }

    #[tokio::test]
    async fn test_today_stats() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        let result = rc.today_stats().await;
        assert!(result.contains("Idag har det postats 0 länkar"));

        // Add a URL for today
        let u1 = url::Url::parse("https://example.com").unwrap();
        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();

        let result = rc.today_stats().await;
        assert!(result.contains("Idag har det postats 1 länkar"));
    }

    #[tokio::test]
    async fn test_top_domains_by_user() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Test empty database
        let result = rc.top_domains_by_user().await;
        assert!(result.contains("Inga domäner hittades"));

        // Add URLs from different users and domains
        let u1 = url::Url::parse("https://github.com/test1").unwrap();
        let u2 = url::Url::parse("https://github.com/test2").unwrap();
        let u3 = url::Url::parse("https://stackoverflow.com/q1").unwrap();
        let u4 = url::Url::parse("https://reddit.com/post1").unwrap();
        let u5 = url::Url::parse("https://github.com/test3").unwrap();
        let u6 = url::Url::parse("https://stackoverflow.com/q2").unwrap();

        // User 123 posts to github (3 times) and stackoverflow (1 time)
        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u2, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u5, 123.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u3, 123.into(), 1u64.into()).await.unwrap();

        // User 456 posts to stackoverflow (2 times) and reddit (1 time)
        rc.add_url(&u6, 456.into(), 1u64.into()).await.unwrap();
        rc.add_url(&u3, 456.into(), 1u64.into()).await.unwrap(); // repost
        rc.add_url(&u4, 456.into(), 1u64.into()).await.unwrap();

        let result = rc.top_domains_by_user().await;
        assert!(result.contains("Top domäner per användare:"));
        assert!(result.contains("<@123>: github.com (3), stackoverflow.com (1)"));
        assert!(result.contains("<@456>: stackoverflow.com (2), reddit.com (1)"));
    }

    #[tokio::test]
    async fn test_url_sanitization() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Add a URL with query string and fragment
        let u1 =
            url::Url::parse("https://example.com/page?utm_source=twitter&foo=bar#section").unwrap();
        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();

        // Try to check for the same URL with different query params - should match by default
        let u2 = url::Url::parse("https://example.com/page?different=params").unwrap();
        let result = rc.check_repost(&u2, 1u64.into()).await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("har postats 1 gång"));

        // Try with no query string at all - should also match
        let u3 = url::Url::parse("https://example.com/page").unwrap();
        let result = rc.check_repost(&u3, 1u64.into()).await;
        assert!(result.is_some());

        // Try with fragment only - should also match
        let u4 = url::Url::parse("https://example.com/page#different-section").unwrap();
        let result = rc.check_repost(&u4, 1u64.into()).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_preserve_full_url_hosts() {
        let rc = RepostChecker::new_with_url("sqlite::memory:")
            .await
            .unwrap();

        // Add example.com to preserve_full_url_hosts
        rc.admin_add_preserve_full_url("example.com").await.unwrap();

        // Add a URL with query string
        let u1 = url::Url::parse("https://example.com/page?id=123").unwrap();
        rc.add_url(&u1, 123.into(), 1u64.into()).await.unwrap();

        // Try to check for the same URL with different query params - should NOT match
        let u2 = url::Url::parse("https://example.com/page?id=456").unwrap();
        let result = rc.check_repost(&u2, 1u64.into()).await;
        assert!(result.is_none());

        // Try with exact same URL - should match
        let u3 = url::Url::parse("https://example.com/page?id=123").unwrap();
        let result = rc.check_repost(&u3, 1u64.into()).await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("har postats 1 gång"));

        // Add the second URL
        rc.add_url(&u2, 456.into(), 1u64.into()).await.unwrap();

        // Now check again - should only find the exact match
        let result = rc.check_repost(&u2, 1u64.into()).await;
        assert!(result.is_some());

        let result_text = result.unwrap();
        assert!(result_text.contains("har postats 1 gång")); // Only 1 match, not 2
    }
}
