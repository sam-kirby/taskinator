# Taskinator

## Overview

Taskinator is a Discord bot that automatically mutes and unmutes players on Discord during a game of Among Us.

At the beginning of the game, all players are automatically muted. When a meeting is called, the bot automatically unmutes anyone who is still alive - usually within 2 seconds. At the end of a meeting, the bot will either mute players if they are still alive, or move them to a separate channel where the dead players can hang out and openly discuss what's going on. At the end of the game all players are unmuted an moved back to the original channel.

The bot must be run by the Among Us game host or it will fail to detect the end of meetings.

The bot must be able to match Discord Users to Among Us Players. If your nickname is the same on Discord as your player name in Among Us, it does this automatically. Otherwise you can use the `~ident <IN_GAME_NAME>` to set your alias. Use the `~check` command to confirm all players are matched to Discord users.

## Configuration

To run the bot, you need to create a `Config.toml` file in the directory you are running it from; it needs the following fields:

```toml
token = "BOT_TOKEN"
living_channel = "VOICE_CHANNEL_ID"
dead_channel = "VOICE_CHANNEL_ID"
broadcast_channel = "TEXT_CHANNEL_ID"
```

The `token` is your Discord bot token. Make sure you add the bot user to the server you are chatting in with appropriate permissions.

The `living_channel` and `dead_channel` are the IDs of the channels which the bot will moderate. You can get a channel ID by turning on developer mode in Discord, then right clicking the channel name and choosing Copy ID.

The `spectator_role` is important if you have more than 10 people on the server. Due to Discord's ratelimiting, if you have more than 10 users in a channel the bot can become very slow; by setting a `spectator_role` you can prevent the bot trying to moderate people who are not playing the game. **Note:** bots are automatically excluded, so no need to give music bots this role.

## Running

Builds are provided via Github Actions. Simply download the executable and place it in the same directory as the config file before running it.

## Building

### Requirements

- Rust compiler (>1.48.0)
- Git or a copy of the source code

### Steps

1. Navigate to the source directory
2. Create the configuration file as described above
3. Execute `cargo run --release`
