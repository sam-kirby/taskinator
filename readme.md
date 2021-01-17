# Taskinator 5000

## Overview

This is a simple Discord bot to assist with running a game of Among Us! It's here to help you stay focussed on who was near the body, not whether you remembered to unmute yourself on Discord.

Only one person needs the bot installed, and is designed to help when you have players who are less tech savvy.

When a game begins, one player needs to execute the `~new` command. They are now the game master, and all players will be muted after 5 seconds.

When a body is found, or an emergency meeting begins, the game master needs to click the 🔴 reaction to the bots message. This will unmute all players who are still alive and move dead players back to the channel.

When the meeting is over, the game master should remove their reaction on 🔴 and the bot will again mute living players. Dead players will be moved to a separate channel where they can freely discuss the game.

A player marks themself as dead by clicking the 💀 reaction during a meeting (immediately muting them), or for those less techy players an admin can run `~dead @Player` to achieve the same effect.

## Configuration

To run the bot, you need to create a `Config.toml` file in the directory you are running it from; it needs the following fields:

```toml
token = "BOT_TOKEN"
living_channel = "CHANNEL_ID"
dead_channel = "CHANNEL_ID"
spectator_role = "ROLE_ID"
```

The `token` is your Discord bot token. Make sure you add the bot user to the server you are chatting in.

The `living_channel` and `dead_channel` are the IDs of the channels which the bot will moderate. You can get a channel ID by turning on developer mode in Discord, then right clicking the channel name and choosing Copy ID.

The `spectator_role` is important if you have more than 10 people on the server. Due to Discord's ratelimiting, if you have more than 10 users in a channel the bot can become very slow; by setting a `spectator_role` you can prevent the bot trying to moderate people who are not playing the game. **Note:** bots are automatically excluded, so no need to give music bots this role.

## Running

Builds are provided via Github Actions. Simply download the executable for your OS and place it in the same directory as the config file before running it.

## Building

### Requirements

* Rust compiler (>1.48.0)
* Git or a copy of the source code

### Steps

1) Navigate to the source directory
2) Create the configuration file as described above
3) Execute `cargo run --release`

## Possible future additions

* Automatic detection of emergency meetings and dead players.
