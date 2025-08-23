use crate::repost_checker::RepostChecker;

use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::sync::Arc;

/// The [`Handler`] implements the Discord trait to listen for messages and reactions.
pub struct Handler {
    pub repost_checker: Arc<RepostChecker>,
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
        if message.mentions.iter().any(|u| u.id == bot_id) {
            if message.content.ends_with("stats") {
                return vec![self.repost_checker.stats().await];
            } else if message.content.ends_with("top domains") {
                return vec![self.repost_checker.top_domains().await];
            } else if message.content.ends_with("top users") {
                return vec![self.repost_checker.top_users().await];
            } else if message.content.ends_with("today") {
                return vec![self.repost_checker.today_stats().await];
            }
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

