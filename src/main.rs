use std::{collections::HashSet, env, sync::Arc, time::Duration};

use serenity::{
    async_trait,
    client::bridge::gateway::{GatewayIntents, ShardManager},
    framework::standard::{
        help_commands,
        macros::{command, group, help},
        Args, CommandGroup, CommandResult, HelpOptions, StandardFramework,
    },
    http::Http,
    model::{
        channel::{Message, Reaction, ReactionType},
        guild::Member,
        id::{ChannelId, MessageId, UserId},
    },
    prelude::*,
};

use tokio::{sync::Mutex, time::delay_for};

const LIVING_CHANNEL: ChannelId = ChannelId(774_309_011_083_493_407);
const DEAD_CHANNEL: ChannelId = ChannelId(774_309_106_995_036_212);

const EMERGENCY_MEETING_EMOJI: &str = "🔴";
const DEAD_EMOJI: &str = "💀";

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn reaction_add(&self, ctx: Context, add_reaction: Reaction) {
        let game = {
            let data = ctx.data.read().await;
            match data.get::<GameContextContainer>() {
                Some(game) => game.clone(),
                None => return,
            }
        };

        let mut game = game.lock().await;

        if add_reaction.message_id != game.ctrl_msg {
            return;
        }

        if let ReactionType::Unicode(emoji) = add_reaction.emoji {
            if emoji == EMERGENCY_MEETING_EMOJI && add_reaction.user_id.unwrap() == game.ctrl_user {
                let living_players = get_connected_members(&ctx, LIVING_CHANNEL).await.unwrap();

                futures::future::join_all(
                    living_players
                        .iter()
                        .filter(|p| !game.dead.contains(&p.user.id))
                        .map(|p| p.edit(&ctx, |p| p.mute(false))),
                )
                .await;

                let dead_players = get_connected_members(&ctx, DEAD_CHANNEL).await.unwrap();

                futures::future::join_all(
                    dead_players
                        .iter()
                        .map(|p| p.edit(&ctx, |p| p.voice_channel(LIVING_CHANNEL).mute(true))),
                )
                .await;

                game.meeting_in_progress = true;
            } else if emoji == DEAD_EMOJI {
                game.dead.insert(add_reaction.user_id.unwrap());

                if game.meeting_in_progress {
                    add_reaction
                        .guild_id
                        .unwrap()
                        .member(&ctx, add_reaction.user_id.unwrap())
                        .await
                        .unwrap()
                        .edit(&ctx, |p| p.mute(true))
                        .await
                        .unwrap();
                }
            }
        }
    }

    async fn reaction_remove(&self, ctx: Context, removed_reaction: Reaction) {
        let game = {
            let data = ctx.data.read().await;
            match data.get::<GameContextContainer>() {
                Some(game) => game.clone(),
                None => return,
            }
        };

        let mut game = game.lock().await;

        if removed_reaction.message_id != game.ctrl_msg {
            return;
        }

        if let ReactionType::Unicode(emoji) = removed_reaction.emoji {
            if emoji == EMERGENCY_MEETING_EMOJI
                && removed_reaction.user_id.unwrap() == game.ctrl_user
            {
                game.meeting_in_progress = false;

                let all_players = get_connected_members(&ctx, LIVING_CHANNEL).await.unwrap();

                futures::future::join_all(
                    all_players
                        .iter()
                        .filter(|p| !p.user.bot && game.dead.contains(&p.user.id))
                        .map(|p| p.edit(&ctx, |p| p.mute(false).voice_channel(DEAD_CHANNEL))),
                )
                .await;
                futures::future::join_all(
                    all_players
                        .iter()
                        .filter(|p| !p.user.bot && !game.dead.contains(&p.user.id))
                        .map(|p| p.edit(&ctx, |p| p.mute(true))),
                )
                .await;
            }
        }
    }
}

struct Game {
    dead: HashSet<UserId>,
    ctrl_msg: MessageId,
    ctrl_user: UserId,
    meeting_in_progress: bool,
}

struct GameContextContainer;

impl TypeMapKey for GameContextContainer {
    type Value = Arc<Mutex<Game>>;
}

struct ShardManagerContainer;

impl TypeMapKey for ShardManagerContainer {
    type Value = Arc<Mutex<ShardManager>>;
}

#[group]
#[only_in(guilds)]
#[owners_only]
#[commands(new, end, stop, dead)]
struct Control;

/// Starts a new game
#[command]
async fn new(ctx: &Context, msg: &Message) -> CommandResult {
    msg.delete(&ctx).await?;
    let ctrl_msg = msg.channel_id.say(&ctx, format!("When a meeting begins, {} should react to this message with :red_circle:.\nWhen it ends, they should unreact\nIf you died during a round, react with :skull: **at the start of the next meeting**!", msg.author.mention())).await?;
    ctrl_msg
        .react(
            &ctx,
            ReactionType::Unicode(EMERGENCY_MEETING_EMOJI.to_string()),
        )
        .await?;
    ctrl_msg
        .react(&ctx, ReactionType::Unicode(DEAD_EMOJI.to_string()))
        .await?;

    {
        let mut data = ctx.data.write().await;
        data.insert::<GameContextContainer>(Arc::new(Mutex::new(Game {
            dead: HashSet::new(),
            ctrl_msg: ctrl_msg.id,
            ctrl_user: msg.author.id,
            meeting_in_progress: false,
        })));
    }

    let notify = msg
        .channel_id
        .say(&ctx, "All players will be muted in 5 seconds")
        .await?;

    delay_for(Duration::from_secs(5)).await;

    notify.delete(&ctx).await.unwrap();

    let members = get_connected_members(&ctx, LIVING_CHANNEL).await?;

    futures::future::join_all(
        members
            .iter()
            .filter(|m| !m.user.bot)
            .map(|m| m.edit(&ctx, |m| m.mute(true))),
    )
    .await;

    Ok(())
}

async fn get_connected_members(
    ctx: &Context,
    channel: ChannelId,
) -> Result<Vec<Member>, serenity::Error> {
    channel
        .to_channel(&ctx)
        .await?
        .guild()
        .unwrap()
        .members(&ctx)
        .await
}

#[command]
async fn end(ctx: &Context, msg: &Message) -> CommandResult {
    msg.delete(&ctx).await?;

    let game = {
        let data = ctx.data.read().await;
        match data.get::<GameContextContainer>() {
            Some(game) => game.clone(),
            None => return Ok(()),
        }
    };

    let game = game.lock().await;

    msg.channel_id
        .message(&ctx, game.ctrl_msg)
        .await?
        .delete(&ctx)
        .await?;

    let living_players = get_connected_members(&ctx, LIVING_CHANNEL).await?;
    futures::future::join_all(
        living_players
            .iter()
            .map(|p| p.edit(&ctx, |p| p.mute(false))),
    )
    .await;

    let dead_players = get_connected_members(&ctx, DEAD_CHANNEL).await?;
    futures::future::join_all(
        dead_players
            .iter()
            .map(|p| p.edit(&ctx, |p| p.voice_channel(LIVING_CHANNEL))),
    )
    .await;

    Ok(())
}

/// Shuts the bot down
#[command]
async fn stop(ctx: &Context, msg: &Message) -> CommandResult {
    msg.delete(&ctx).await?;

    msg.channel_id.say(&ctx, "Shutting down bot").await?;

    let data = ctx.data.read().await;
    data.get::<ShardManagerContainer>()
        .expect("Failed to retrieve ShardManager from data")
        .lock()
        .await
        .shutdown_all()
        .await;

    Ok(())
}

/// Make someone dead
#[command]
async fn dead(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
    msg.delete(&ctx).await?;

    let newly_dead = args.parse::<UserId>()?;
    let newly_dead = msg.guild_id.unwrap().member(&ctx, newly_dead).await?;

    let reply = msg
        .channel_id
        .say(&ctx, format!("Deadifying {}", newly_dead.display_name()))
        .await?;

    let game = {
        let data = ctx.data.read().await;
        data.get::<GameContextContainer>()
            .expect("Failed to get game data, make sure a game has started")
            .clone()
    };

    let mut game = game.lock().await;

    game.dead.insert(newly_dead.user.id);

    if game.meeting_in_progress {
        newly_dead.edit(&ctx, |nd| nd.mute(true)).await?;
    }

    delay_for(Duration::from_secs(5)).await;
    reply.delete(&ctx).await?;

    Ok(())
}

#[help]
async fn my_help(
    ctx: &Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    let _ = help_commands::with_embeds(ctx, msg, args, help_options, groups, owners).await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    tracing_subscriber::fmt::init();

    let token = env::var("DISCORD_TOKEN")?;

    let (owners, bot_id) = {
        let http = Http::new_with_token(&token);

        // Determine owner of bot
        let info = http.get_current_application_info().await?;
        let mut owners = HashSet::new();

        if let Some(team) = info.team {
            owners.extend(team.members.iter().map(|team_member| team_member.user.id));
        } else {
            owners.insert(info.owner.id);
        }

        let bot_user = http.get_current_user().await?;

        (owners, bot_user.id)
    };

    let framework = StandardFramework::new()
        .configure(|c| c.on_mention(Some(bot_id)).prefix("~").owners(owners))
        .help(&MY_HELP)
        .group(&CONTROL_GROUP);

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .intents(
            GatewayIntents::GUILDS
                | GatewayIntents::GUILD_MESSAGES
                | GatewayIntents::GUILD_MESSAGE_REACTIONS
                | GatewayIntents::GUILD_VOICE_STATES,
        )
        .framework(framework)
        .await?;

    {
        let mut data = client.data.write().await;
        data.insert::<ShardManagerContainer>(client.shard_manager.clone());
    }

    client.start().await?;

    Ok(())
}
