use std::{collections::HashSet, time::Duration};

mod config;
mod context;

use config::Config;
use context::Context;

use futures::StreamExt;
use tokio::time::sleep;
use tracing::error;
use twilight_cache_inmemory::InMemoryCache as DiscordCache;
use twilight_command_parser::{Command, CommandParserConfig, Parser};
use twilight_gateway::{shard::Shard, EventTypeFlags, Intents};
use twilight_http::{request::channel::reaction::RequestReactionType, Client as DiscordHttp};
use twilight_mention::{Mention, ParseMention};
use twilight_model::{channel::Message, channel::ReactionType, gateway::event::Event, id::UserId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

const EMER_EMOJI: &str = "🔴";
const DEAD_EMOJI: &str = "💀";

#[tokio::main]
async fn main() -> Result<()> {
    // Setup
    tracing_subscriber::fmt::init();

    let config = Config::from_file("./config.toml").await?;

    let cache = DiscordCache::builder().message_cache_size(5).build();

    let discord_http = DiscordHttp::new(&config.token);

    let owners = {
        let mut owners = HashSet::new();

        let app_info = discord_http.current_user_application().await?;
        if let Some(team) = app_info.team {
            owners.extend(team.members.iter().map(|tm| tm.user.id));
        } else {
            owners.insert(app_info.owner.id);
        }
        owners
    };

    let mut shard = Shard::new(
        &config.token,
        Intents::GUILDS
            | Intents::GUILD_MESSAGES
            | Intents::GUILD_MESSAGE_REACTIONS
            | Intents::GUILD_VOICE_STATES,
    );
    let shutdown_handle = shard.clone();

    // Start gateway
    shard.start().await?;

    let event_flags: EventTypeFlags = EventTypeFlags::GUILD_CREATE
        | EventTypeFlags::MESSAGE_CREATE
        | EventTypeFlags::MESSAGE_DELETE
        | EventTypeFlags::REACTION_ADD
        | EventTypeFlags::REACTION_REMOVE
        | EventTypeFlags::VOICE_STATE_UPDATE;

    let mut events = shard.some_events(event_flags);

    let mut context = Context::new(config, discord_http, cache, shutdown_handle, owners);

    let parser = {
        let mut parser_config = CommandParserConfig::new();
        parser_config.add_prefix("~");
        parser_config.add_command("new", false);
        parser_config.add_command("end", false);
        parser_config.add_command("dead", false);
        parser_config.add_command("stop", false);

        Parser::new(parser_config)
    };

    // Gateway event loop
    while let Some(event) = events.next().await {
        context.cache.update(&event);

        match event {
            Event::MessageCreate(event) => {
                let context_clone = context.clone();
                let parser_clone = parser.clone();
                tokio::spawn(async move {
                    if let Err(e) = process_command(context_clone, parser_clone, (*event).0).await {
                        error!("{}", e);
                    }
                });
            }
            Event::ReactionAdd(event) => {
                let reaction = (*event).0;

                if context.is_reacting_to_control(&reaction).await {
                    match reaction.emoji {
                        ReactionType::Unicode { ref name } if name == EMER_EMOJI => {
                            if context.is_in_control(&reaction.user_id).await {
                                context.emergency_meeting().await?;
                            }
                        }
                        ReactionType::Unicode { ref name } if name == DEAD_EMOJI => {
                            context.make_dead(&reaction.user_id).await;
                        }
                        _ => {}
                    }
                }
            }
            Event::ReactionRemove(event) => {
                let reaction = (*event).0;
                if matches!(reaction.emoji, ReactionType::Unicode { ref name } if name == EMER_EMOJI)
                    && context.is_reacting_to_control(&reaction).await
                    && context.is_in_control(&reaction.user_id).await
                {
                    context.mute_players().await?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

async fn process_command(mut ctx: Context, parser: Parser<'_>, msg: Message) -> Result<()> {
    match parser.parse(&msg.content) {
        Some(Command { name: "new", .. }) => {
            ctx.discord_http
                .delete_message(msg.channel_id, msg.id)
                .await?;

            let ctrl_msg = ctx.discord_http
            .create_message(msg.channel_id)
            .content(format!(
                r#"A game is in progress, {} can react to this message with {} to call a meeting.
Anyone can react to this message with {} to access dead chat after the next meeting"#,
                msg.author.mention(),
                EMER_EMOJI,
                DEAD_EMOJI
            ))?
            .await?;

            let emer_emoji = RequestReactionType::Unicode {
                name: EMER_EMOJI.into(),
            };
            ctx.discord_http
                .create_reaction(ctrl_msg.channel_id, ctrl_msg.id, emer_emoji)
                .await?;

            let dead_emoji = RequestReactionType::Unicode {
                name: DEAD_EMOJI.into(),
            };
            ctx.discord_http
                .create_reaction(ctrl_msg.channel_id, ctrl_msg.id, dead_emoji)
                .await?;

            ctx.start_game(&ctrl_msg, msg.guild_id.unwrap()).await;

            sleep(Duration::from_secs(5)).await;

            ctx.mute_players().await?;
        }
        Some(Command { name: "end", .. }) => {
            ctx.discord_http
                .delete_message(msg.channel_id, msg.id)
                .await?;

            let auth = ctx.is_in_control(&msg.author.id).await;
            if auth {
                ctx.end_game().await?;
            }
        }
        Some(Command {
            name: "dead",
            mut arguments,
            ..
        }) => {
            ctx.discord_http
                .delete_message(msg.channel_id, msg.id)
                .await?;

            let auth = ctx.is_in_control(&msg.author.id).await;
            if auth {
                match arguments.next().map(UserId::parse) {
                    Some(Ok(target)) => {
                        let reply = ctx
                            .broadcast()
                            .await
                            .unwrap()
                            .content(format!("deadifying {}", target.mention()))?
                            .await?;
                        ctx.make_dead(&target).await;
                        sleep(Duration::from_secs(5)).await;
                        ctx.discord_http
                            .delete_message(reply.channel_id, reply.id)
                            .await?;
                    }
                    _ => {
                        ctx.broadcast()
                            .await
                            .unwrap()
                            .content("You must mention the user you wish to die")?
                            .await?;
                    }
                }
            } else if let Some(broadcast) = ctx.broadcast().await {
                broadcast.content("You must have started the game or be an owner of the bot to make others dead\nTo make yourself dead, please use the reactions")?.await?;
            } else {
                ctx.discord_http
                    .create_message(msg.channel_id)
                    .content("There is no game running")?
                    .await?;
            }
        }
        Some(Command { name: "stop", .. }) => {
            ctx.discord_http
                .delete_message(msg.channel_id, msg.id)
                .await?;

            if ctx.is_game_in_progress().await {
                ctx.end_game().await?;
            }

            ctx.shard.shutdown();
        }
        _ => {}
    }

    Ok(())
}
