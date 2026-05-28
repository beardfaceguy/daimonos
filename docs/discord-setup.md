# Discord Integration Setup

This guide covers Phase 1 Discord integration for Daimonos: read-only access to
allowlisted guilds/channels via bot token auth.

Current tool surface:

- `discord` `command=list_guilds`
- `discord` `command=list_channels`
- `discord` `command=read_messages`
- `discord` `command=search_messages`

## Security Model (Phase 1)

Daimonos Discord integration is locked down by default:

- Disabled by default (`[discord].enabled = false`)
- Explicit allowlists required for guild and channel IDs
- Read-only default mode enabled
- Message/response size limits enforced
- Mention sanitization (`@everyone`, `@here`, mention tags)
- Attachment metadata only (no file downloads)

No self-bot/user-token mode is supported. Use a Discord bot token only.

## Prerequisites

- Daimonos installed and configured as your MCP server
- A Discord application with a bot user
- Admin access to install the bot into target guild(s)

## 1) Create Discord Bot Credentials

1. Open the [Discord Developer Portal](https://discord.com/developers/applications).
2. Create or select your application.
3. In **Bot**:
   - create/reset bot token
   - copy token (store it securely; do not commit it)
4. In **OAuth2 > URL Generator**:
   - scope: `bot`
   - install bot into your target guild

Recommended minimum bot permissions for Phase 1 read-only flow:

- View Channels
- Read Message History

## 2) Set the Bot Token in Environment

Use the default variable name:

```bash
export DISCORD_BOT_TOKEN="your-bot-token"
```

Or set a custom variable and reference it via `bot_token_env_var` in config.

## 3) Configure Daimonos Discord Section

Add to `daimonos.toml` (workspace) or `~/.config/daimonos/config.toml`:

```toml
[discord]
enabled = true
bot_token_env_var = "DISCORD_BOT_TOKEN"
api_base_url = "https://discord.com/api/v10"

# Explicit allowlists (required for access)
allow_guild_ids = ["123456789012345678"]
allow_channel_ids = ["223456789012345678"]

# Output bounds
max_messages_per_call = 100
max_message_chars = 4000
max_response_chars = 32000

# Safety defaults
read_only_default = true
rate_limit_max_retries = 2
rate_limit_max_sleep_ms = 10000
```

## 4) Find Guild/Channel IDs

In Discord client:

1. Enable **Developer Mode** in advanced settings.
2. Right-click guild or channel.
3. Click **Copy Server ID** / **Copy Channel ID**.

Paste these values into the allowlists above.

## 5) Verify in MCP

Start a new agent session and run a simple call:

- `discord` with `command=list_guilds`
- `discord` with `command=list_channels` and a allowlisted `guild_id`
- `discord` with `command=read_messages` and a allowlisted `channel_id`

Expected behavior:

- non-allowlisted resources return deterministic permission errors
- responses include `observability` fields for rate-limit visibility
- content is sanitized and bounded

## Troubleshooting

### "discord integration disabled"

- Set `[discord].enabled = true`
- restart the MCP session so config is reloaded

### "env var ... is not set"

- export `DISCORD_BOT_TOKEN` in the environment used to launch your editor/agent
- confirm `bot_token_env_var` matches the env var name

### "guild/channel ... is not allowlisted"

- add the exact snowflake IDs to `allow_guild_ids` / `allow_channel_ids`

### 429 / rate limit issues

- increase `rate_limit_max_retries` and/or `rate_limit_max_sleep_ms`
- reduce `max_messages_per_call` for lower request load
