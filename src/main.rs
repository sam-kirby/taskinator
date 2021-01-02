use std::{collections::HashSet, path::Path, str::from_utf8, sync::Arc, time::Duration};

use futures::StreamExt;
use serde::Deserialize;
use tokio::{fs::File, io::AsyncReadExt, sync::RwLock, time::delay_for};
use tracing::warn;
use twilight_cache_inmemory::{model::CachedMember, InMemoryCache as DiscordCache};
use twilight_command_parser::{Command, CommandParserConfig, Parser};
use twilight_gateway::{shard::Shard, EventTypeFlags, Intents};
use twilight_http::{request::channel::reaction::RequestReactionType, Client as DiscordHttp};
use twilight_mention::{Mention, ParseMention};
use twilight_model::{
    channel::GuildChannel,
    channel::Message,
    channel::ReactionType,
    gateway::event::Event,
    id::{ChannelId, MessageId, RoleId, UserId},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

const EMERGENCY_MEETING_EMOJI: &str = "🔴";
const DEAD_EMOJI: &str = "💀";

#[derive(Deserialize)]
struct Config {
    token: String,
    living_channel: ChannelId,
    dead_channel: ChannelId,
    spectator_role: RoleId,
    mode: Mode,
}

impl Config {
    async fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = File::open(path.as_ref()).await?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await?;

        let config_str = from_utf8(&contents)?;

        let config: Config = toml::from_str(config_str)?;

        Ok(config)
    }
}

#[derive(Deserialize, PartialEq)]
enum Mode {
    Deafen,
    Mute,
}

#[derive(Clone)]
struct Context {
    config: Arc<Config>,
    discord_http: DiscordHttp,
    cache: DiscordCache,
    owners: Arc<HashSet<UserId>>,
    game: Arc<RwLock<Option<Game>>>,
}

struct Game {
    dead: HashSet<UserId>,
    ctrl_channel: ChannelId,
    ctrl_msg: MessageId,
    ctrl_user: UserId,
    meeting_in_progress: bool,
}

impl Game {
    fn new(ctrl_msg: Message, ctrl_user: UserId) -> Self {
        Self {
            dead: HashSet::new(),
            ctrl_channel: ctrl_msg.channel_id,
            ctrl_msg: ctrl_msg.id,
            ctrl_user,
            meeting_in_progress: false,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
        Arc::new(owners)
    };

    let mut shard = Shard::new(
        &config.token,
        Intents::GUILDS
            | Intents::GUILD_MESSAGES
            | Intents::GUILD_MESSAGE_REACTIONS
            | Intents::GUILD_VOICE_STATES,
    );

    shard.start().await?;

    let event_flags: EventTypeFlags = EventTypeFlags::GUILD_CREATE
        | EventTypeFlags::MESSAGE_CREATE
        | EventTypeFlags::MESSAGE_DELETE
        | EventTypeFlags::REACTION_ADD
        | EventTypeFlags::REACTION_REMOVE
        | EventTypeFlags::VOICE_STATE_UPDATE;

    let mut events = shard.some_events(event_flags);

    let context = Context {
        config: Arc::new(config),
        discord_http,
        cache,
        owners,
        game: Arc::new(RwLock::new(None)),
    };

    let parser = {
        let mut parser_config = CommandParserConfig::new();
        parser_config.add_prefix("~");
        parser_config.add_command("new", false);
        parser_config.add_command("end", false);
        parser_config.add_command("dead", false);

        Parser::new(parser_config)
    };

    while let Some(event) = events.next().await {
        context.cache.update(&event);

        match event {
            Event::MessageCreate(event) => {
                let context_clone = context.clone();
                let parser_clone = parser.clone();
                tokio::spawn(async move {
                    process_command(context_clone, parser_clone, (*event).0).await
                });
            }
            Event::ReactionAdd(event) => {
                let reaction = (*event).0;
                if let ReactionType::Unicode { ref name } = reaction.emoji {
                    if name == EMERGENCY_MEETING_EMOJI {
                        let auth = {
                            let game = context.game.read().await;
                            game.is_some()
                                && game
                                    .as_ref()
                                    .map(|g| {
                                        g.ctrl_user == reaction.user_id
                                            && g.ctrl_msg == reaction.message_id
                                    })
                                    .unwrap()
                        };
                        if auth {
                            emergency_meeting(context.clone()).await?;
                        }
                    } else if name == DEAD_EMOJI {
                        let auth = {
                            let game = context.game.read().await;
                            game.is_some()
                                && game
                                    .as_ref()
                                    .map(|g| g.ctrl_msg == reaction.message_id)
                                    .unwrap()
                        };
                        if auth {
                            if let Some(g) = context.game.write().await.as_mut() {
                                g.dead.insert(reaction.user_id);
                            }
                            if context
                                .game
                                .read()
                                .await
                                .as_ref()
                                .map(|g| g.meeting_in_progress)
                                .unwrap()
                            {
                                context
                                    .discord_http
                                    .update_guild_member(
                                        reaction.guild_id.unwrap(),
                                        reaction.user_id,
                                    )
                                    .mute(true)
                                    .await?;
                            }
                        }
                    }
                }
            }
            Event::ReactionRemove(event) => {
                let reaction = (*event).0;
                if let ReactionType::Unicode { ref name } = reaction.emoji {
                    if name == EMERGENCY_MEETING_EMOJI {
                        let auth = {
                            let game = context.game.read().await;
                            game.is_some()
                                && game
                                    .as_ref()
                                    .map(|g| {
                                        g.ctrl_user == reaction.user_id
                                            && g.ctrl_msg == reaction.message_id
                                    })
                                    .unwrap()
                        };
                        if auth {
                            mute_players(context.clone()).await?;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

async fn process_command(ctx: Context, parser: Parser<'_>, msg: Message) -> Result<()> {
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
                EMERGENCY_MEETING_EMOJI,
                DEAD_EMOJI
            ))?
            .await?;

            let emer_emoji = RequestReactionType::Unicode {
                name: EMERGENCY_MEETING_EMOJI.into(),
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

            let game = Game::new(ctrl_msg, msg.author.id);

            ctx.game.write().await.replace(game);

            delay_for(Duration::from_secs(5)).await;

            if ctx.config.mode == Mode::Mute {
                mute_players(ctx).await?;
            } else {
                unimplemented!()
            }
        }
        Some(Command { name: "end", .. }) => {
            ctx.discord_http
                .delete_message(msg.channel_id, msg.id)
                .await?;

            let auth = {
                let game = ctx.game.read().await;
                game.is_some() && game.as_ref().map(|g| g.ctrl_user == msg.author.id).unwrap()
            };
            if auth {
                let game = ctx.game.write().await.take();
                if let Some(game) = game {
                    ctx.discord_http
                        .delete_message(game.ctrl_channel, game.ctrl_msg)
                        .await?;
                    end_game(ctx).await?;
                }
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

            let auth = {
                let game = ctx.game.read().await;
                game.is_some() && game.as_ref().map(|g| g.ctrl_user == msg.author.id).unwrap()
            };
            if auth {
                if let Some(target) = arguments.next().and_then(|t| UserId::parse(t).ok()) {
                    let success = {
                        let mut game = ctx.game.write().await;
                        if game.is_some() {
                            if let Some(g) = game.as_mut() {
                                g.dead.insert(target);
                            }
                            true
                        } else {
                            false
                        }
                    };

                    if success {
                        let notify = ctx
                            .discord_http
                            .create_message(msg.channel_id)
                            .content(format!("deadifying {}", target.mention()))?
                            .await?;
                        match ctx.cache.member(msg.guild_id.unwrap(), target) {
                            Some(member) if !member.mute => {
                                ctx.discord_http
                                    .update_guild_member(member.guild_id, member.user.id)
                                    .mute(true)
                                    .await?;
                            }
                            Some(member) => {}
                            None => warn!("cache miss: member will not be muted"),
                        }
                        delay_for(Duration::from_secs(5)).await;
                        ctx.discord_http
                            .delete_message(notify.channel_id, notify.id)
                            .await?;
                    } else {
                        ctx.discord_http
                            .create_message(msg.channel_id)
                            .content("No game is in progress")?
                            .await?;
                    }
                } else {
                    ctx.discord_http
                        .create_message(msg.channel_id)
                        .content("Must specify target")?
                        .await?;
                }
            } else {
                ctx.discord_http
                    .create_message(msg.channel_id)
                    .content("You do not have permission to make people dead")?
                    .await?;
            }
        }
        Some(_) => {}
        None => {}
    }

    Ok(())
}

async fn get_members_in_channel(
    ctx: &Context,
    voice_channel: Arc<GuildChannel>,
) -> Vec<Arc<CachedMember>> {
    match ctx.cache.voice_channel_states(voice_channel.id()) {
        Some(vs) => vs
            .iter()
            .map(|vs| ctx.cache.member(vs.guild_id.unwrap(), vs.user_id).unwrap())
            .filter(|m| !m.user.bot && !m.roles.contains(&ctx.config.spectator_role))
            .collect(),
        None => Vec::new(),
    }
}

async fn mute_players(ctx: Context) -> Result<()> {
    let living_channel = ctx.cache.guild_channel(ctx.config.living_channel).unwrap();
    for member in get_members_in_channel(&ctx, living_channel).await {
        let mut futures = Vec::new();

        if ctx
            .game
            .read()
            .await
            .as_ref()
            .unwrap()
            .dead
            .contains(&member.user.id)
        {
            futures.push(
                ctx.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .channel_id(ctx.config.dead_channel)
                    .mute(false),
            );
        } else {
            futures.push(
                ctx.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .mute(true),
            )
        }

        futures::future::join_all(futures).await; // TODO: handle errors, particularly 429 ratelimits

        if let Some(g) = ctx.game.write().await.as_mut() {
            g.meeting_in_progress = false
        }
    }

    Ok(())
}

async fn emergency_meeting(ctx: Context) -> Result<()> {
    let living_channel = ctx.cache.guild_channel(ctx.config.living_channel).unwrap();
    let dead_channel = ctx.cache.guild_channel(ctx.config.dead_channel).unwrap();

    let mut futures = Vec::new();

    for member in get_members_in_channel(&ctx, living_channel).await {
        futures.push(
            ctx.discord_http
                .update_guild_member(member.guild_id, member.user.id)
                .mute(false),
        );
    }

    for member in get_members_in_channel(&ctx, dead_channel).await {
        futures.push(
            ctx.discord_http
                .update_guild_member(member.guild_id, member.user.id)
                .channel_id(ctx.config.living_channel)
                .mute(true),
        )
    }

    futures::future::join_all(futures).await; // TODO: handle errors, particularly 429 ratelimits

    if let Some(g) = ctx.game.write().await.as_mut() {
        g.meeting_in_progress = true
    }

    Ok(())
}

async fn end_game(ctx: Context) -> Result<()> {
    let living_channel = ctx.cache.guild_channel(ctx.config.living_channel).unwrap();
    let dead_channel = ctx.cache.guild_channel(ctx.config.dead_channel).unwrap();

    let mut futures = Vec::new();

    for member in get_members_in_channel(&ctx, living_channel).await {
        futures.push(
            ctx.discord_http
                .update_guild_member(member.guild_id, member.user.id)
                .mute(false),
        );
    }

    for member in get_members_in_channel(&ctx, dead_channel).await {
        futures.push(
            ctx.discord_http
                .update_guild_member(member.guild_id, member.user.id)
                .channel_id(ctx.config.living_channel),
        );
    }

    futures::future::join_all(futures).await; // TODO: handle errors, particularly 429 ratelimits

    Ok(())
}
