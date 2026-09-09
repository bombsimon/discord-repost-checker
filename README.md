<h1 align="center">
  <img src="repost.png" alt="Repost Checker">
  <br>
  Repost Checker
  <br>
</h1>

Just your regular annoying bot that will notify when someone reposts a link on
Discord and give credit to the people who did it first!

## Usage

Setup the bot, have it join your channel, allow it to read messages. That's it!
Once up and running, you can control your bot via Discord.

### Reactions

You can react to a repost message by the bot to configure specific domains.

- React with one of 👎, 👎🏻, 👎🏼, 👎🏽, 👎🏿 to add the domain to ignore list and not
  enable repost for it. Can not be done if list is in the always-enable list. An
  ignored domain can only be enabled again via admin command.
- React with ❓ to require full URL match, including query string. By default,
  only URL and path is matched, but some sites rely on anchors or query strings
  to serve unique content so this will enable full URL matching. A full URL
  match can only be disabled via admin command.

### Admin commands (DM to bot only)

- `always-enable-add <host>` - Always check reposts for host
- `always-enable-remove <host>` - Remove host from always enabled
- `ignore-add <host>` - Ignore reposts from host
- `ignore-remove <host>` - Remove host from ignore list
- `preserve-full-url-add <host>` - Match full URLs (with query strings) for host
- `preserve-full-url-remove <host>` - Remove host from preserve full URL list
- `list-urls` - List all configured hosts

### Channel commands

- `@bot stats` - Show total unique links and user statistics
- `@bot top domains` - Show top 5 most posted domains
- `@bot top users` - Show top 5 users by link count
- `@bot top user-domains` - Show top domains for each user
- `@bot today` - Show number of links posted today
