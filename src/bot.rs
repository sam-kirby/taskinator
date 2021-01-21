use crate::{
    config::Config,
    constants::{DEAD_EMOJI, EMER_EMOJI},
    Result,
};

use std::{
    collections::HashSet, future::Future, path::Path, result::Result as StdResult, sync::Arc,
    time::Duration,
};

use parking_lot::RwLock;
use tokio::{signal::ctrl_c, task::JoinHandle, time::sleep};
use tracing::error;
use twilight_cache_inmemory::{model::CachedMember, InMemoryCache as DiscordCache, ResourceType};
use twilight_command_parser::{Arguments, Command, CommandParserConfig, Parser};
use twilight_gateway::{shard::Events, Event, EventTypeFlags, Intents, Shard};
use twilight_http::{
    error::Result as TwiResult,
    request::channel::{message::CreateMessage, reaction::RequestReactionType},
    Client as DiscordHttp,
};
use twilight_mention::{Mention, ParseMention};
use twilight_model::{
    channel::GuildChannel,
    channel::{Message, Reaction, ReactionType},
    id::{ChannelId, GuildId, MessageId, UserId},
};

struct Game {
    dead: HashSet<UserId>,
    ctrl_channel: ChannelId,
    ctrl_msg: MessageId,
    ctrl_user: UserId,
    guild_id: GuildId,
    meeting_in_progress: bool,
}

#[derive(Clone)]
pub struct Bot<'p> {
    bot_id: Option<UserId>,
    cache: DiscordCache,
    config: Arc<Config>,
    discord_http: DiscordHttp,
    game: Arc<RwLock<Option<Game>>>,
    owners: Arc<RwLock<Option<HashSet<UserId>>>>,
    parser: Arc<Parser<'p>>,
    shard: Shard,
}

impl Bot<'_> {
    pub fn new(config_path: impl AsRef<Path>) -> Result<Self> {
        let config = Arc::new(Config::from_file(config_path)?);

        let discord_http = DiscordHttp::new(&config.token);

        let cache = DiscordCache::builder()
            .resource_types(
                ResourceType::CHANNEL
                    | ResourceType::GUILD
                    | ResourceType::MEMBER
                    | ResourceType::USER
                    | ResourceType::VOICE_STATE,
            )
            .build();

        let shard = Shard::new(
            &config.token,
            Intents::GUILDS
                | Intents::GUILD_MESSAGES
                | Intents::GUILD_MESSAGE_REACTIONS
                | Intents::GUILD_VOICE_STATES,
        );

        let parser = {
            let mut parser_config = CommandParserConfig::new();
            parser_config.add_prefix("~");
            parser_config.add_command("new", false);
            parser_config.add_command("end", false);
            parser_config.add_command("dead", false);
            parser_config.add_command("stop", false);

            Arc::new(Parser::new(parser_config))
        };

        Ok(Bot {
            bot_id: None,
            cache,
            config,
            discord_http,
            game: Arc::new(RwLock::new(None)),
            owners: Arc::new(RwLock::new(None)),
            parser,
            shard,
        })
    }

    pub async fn fetch_app_details(&mut self) -> Result<()> {
        let (owners, current_user) = {
            let mut owners = HashSet::new();

            let app_info = self.discord_http.current_user_application().await?;
            if let Some(team) = app_info.team {
                owners.extend(team.members.iter().map(|tm| tm.user.id));
            } else {
                owners.insert(app_info.owner.id);
            }
            (owners, UserId(app_info.id.0))
        };

        self.owners.write().replace(owners);
        self.bot_id.replace(current_user);

        Ok(())
    }

    pub async fn start_gateway(&mut self) -> Result<Events> {
        let shutdown_handle = self.shard.clone();
        tokio::spawn(async move {
            if let Err(e) = ctrl_c().await {
                error!("Error registering ctrl+c handler!\n{}", e);
            }

            shutdown_handle.shutdown();
        });

        self.shard.start().await?;

        let event_flags: EventTypeFlags = EventTypeFlags::GUILD_CREATE
            | EventTypeFlags::MESSAGE_CREATE
            | EventTypeFlags::MESSAGE_DELETE
            | EventTypeFlags::REACTION_ADD
            | EventTypeFlags::REACTION_REMOVE
            | EventTypeFlags::VOICE_STATE_UPDATE;

        Ok(self.shard.some_events(event_flags))
    }

    pub fn update_cache(&self, event: &Event) {
        self.cache.update(event);
    }

    pub fn bot_id(&self) -> UserId {
        self.bot_id
            .expect("Expected bot ID - must fetch app info first!")
    }

    pub async fn process_command(&self, msg: &Message) -> Result<()> {
        match self.parser.parse(&msg.content) {
            Some(Command {
                name: "new",
                arguments,
                ..
            }) => {
                self.discord_http
                    .delete_message(msg.channel_id, msg.id)
                    .await?;
                self.begin_game(msg, arguments).await?
            }
            Some(Command { name: "end", .. }) => {
                self.discord_http
                    .delete_message(msg.channel_id, msg.id)
                    .await?;

                if self.is_in_control(msg.author.id) {
                    self.end_game().await?;
                }
            }
            Some(Command {
                name: "dead",
                arguments,
                ..
            }) => {
                self.discord_http
                    .delete_message(msg.channel_id, msg.id)
                    .await?;

                self.deadify(msg, arguments).await?;
            }
            Some(Command { name: "stop", .. }) => {
                self.discord_http
                    .delete_message(msg.channel_id, msg.id)
                    .await?;

                if self.is_in_control(msg.author.id) {
                    if self.is_game_in_progress() {
                        self.end_game().await?;
                    }

                    self.shard.shutdown();
                }
            }
            _ => {}
        }

        Ok(())
    }

    pub async fn reaction_add_handler(&self, reaction: &Reaction) -> Result<()> {
        if self.is_reacting_to_control(&reaction) {
            match reaction.emoji {
                ReactionType::Unicode { ref name } if name == EMER_EMOJI => {
                    if self.is_in_control(reaction.user_id) {
                        self.begin_meeting().await?;
                    }
                }
                ReactionType::Unicode { ref name } if name == DEAD_EMOJI => {
                    self.make_dead(reaction.user_id).await;
                }
                _ => {}
            }
        }

        Ok(())
    }

    pub async fn reaction_remove_handler(&self, reaction: &Reaction) -> Result<()> {
        if self.is_reacting_to_control(&reaction)
            && self.is_in_control(reaction.user_id)
            && matches!(reaction.emoji, ReactionType::Unicode { ref name } if name == EMER_EMOJI)
        {
            self.end_meeting().await?;
        }

        Ok(())
    }

    async fn begin_game(&self, msg: &Message, mut args: Arguments<'_>) -> Result<()> {
        let ctrl_msg = self
            .discord_http
            .create_message(msg.channel_id)
            .content(format!(
                "A game is in progress, {} can react to this message with {} to call a \
             meeting.\nAnyone can react to this message with {} to access dead chat \
             after the next meeting",
                msg.author.mention(),
                EMER_EMOJI,
                DEAD_EMOJI
            ))?
            .await?;

        let discord_http = self.discord_http.clone();
        let reaction_ctrl_msg = ctrl_msg.clone();

        // Adding emoji takes ~1 second; don't hold up starting a game by doing it concurrently
        let res: JoinHandle<Result<()>> = tokio::spawn(async move {
            let emojis = vec![
                RequestReactionType::Unicode {
                    name: EMER_EMOJI.into(),
                },
                RequestReactionType::Unicode {
                    name: DEAD_EMOJI.into(),
                },
            ];

            for emoji in emojis {
                discord_http
                    .create_reaction(reaction_ctrl_msg.channel_id, reaction_ctrl_msg.id, emoji)
                    .await?;
            }

            Ok(())
        });

        self.game.write().replace(Game {
            dead: HashSet::new(),
            ctrl_channel: msg.channel_id,
            ctrl_msg: ctrl_msg.id,
            ctrl_user: msg.author.id,
            guild_id: msg.guild_id.unwrap(),
            meeting_in_progress: false,
        });

        let duration = match args.next().and_then(|s| s.parse().ok()) {
            Some(time) if time == 0 => None,
            Some(time) => Some(Duration::from_secs(time)),
            None => Some(Duration::from_secs(5)),
        };

        if let Some(duration) = duration {
            sleep(duration).await;
        }

        self.end_meeting().await?;

        res.await??;

        Ok(())
    }

    async fn end_game(&self) -> Result<()> {
        let game = self.game.write().take();
        if let Some(game) = game {
            self.discord_http
                .delete_message(game.ctrl_channel, game.ctrl_msg)
                .await?;
        } else {
            return Ok(());
        }

        let living_channel = self
            .cache
            .guild_channel(self.config.living_channel)
            .unwrap();
        let dead_channel = self.cache.guild_channel(self.config.dead_channel).unwrap();

        let mut futures = Vec::new();

        for member in self.get_members_in_channel(&living_channel) {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .mute(false),
            );
        }

        for member in self.get_members_in_channel(&dead_channel) {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .channel_id(self.config.living_channel),
            );
        }

        self.batch(futures).await;

        Ok(())
    }

    async fn begin_meeting(&self) -> Result<()> {
        let living_channel = self
            .cache
            .guild_channel(self.config.living_channel)
            .unwrap();
        let dead_channel = self.cache.guild_channel(self.config.dead_channel).unwrap();

        let mut futures = Vec::new();

        {
            let game_lock = self.game.read();
            let game = game_lock.as_ref().unwrap();

            for member in self.get_members_in_channel(&living_channel) {
                if game.dead.contains(&member.user.id) {
                    continue;
                }
                futures.push(
                    self.discord_http
                        .update_guild_member(member.guild_id, member.user.id)
                        .mute(false),
                );
            }
        }

        for member in self.get_members_in_channel(&dead_channel) {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .channel_id(self.config.living_channel)
                    .mute(true),
            )
        }

        self.batch(futures).await;

        let mut game_lock = self.game.write();
        let g = game_lock.as_mut().expect("expected game");
        g.meeting_in_progress = true;

        Ok(())
    }

    async fn end_meeting(&self) -> Result<()> {
        let living_channel = self
            .cache
            .guild_channel(self.config.living_channel)
            .unwrap();

        let (alive_players, dead_players): (Vec<_>, Vec<_>) = {
            let game_lock = self.game.read();
            let game = game_lock.as_ref().unwrap();

            self.get_members_in_channel(&living_channel)
                .into_iter()
                .partition(|p| !game.dead.contains(&p.user.id))
        };

        let mut futures = Vec::new();

        for player in alive_players {
            futures.push(
                self.discord_http
                    .update_guild_member(player.guild_id, player.user.id)
                    .mute(true),
            );
        }

        for player in dead_players {
            futures.push(
                self.discord_http
                    .update_guild_member(player.guild_id, player.user.id)
                    .channel_id(self.config.dead_channel)
                    .mute(false),
            );
        }

        self.batch(futures).await;

        let mut game_lock = self.game.write();
        let g = game_lock.as_mut().expect("expected game");
        g.meeting_in_progress = false;

        Ok(())
    }

    async fn deadify(&self, msg: &Message, mut args: Arguments<'_>) -> Result<()> {
        if let Some(broadcast) = self.broadcast() {
            if self.is_in_control(msg.author.id) {
                match args.next().map(UserId::parse) {
                    Some(Ok(target)) => {
                        let reply = broadcast
                            .content(format!("deadifying {}", target.mention()))?
                            .await?;
                        self.make_dead(target).await;
                        sleep(Duration::from_secs(5)).await;
                        self.discord_http
                            .delete_message(reply.channel_id, reply.id)
                            .await?;
                    }
                    _ => {
                        broadcast
                            .content("You must mention the user you wish to die")?
                            .await?;
                    }
                }
            } else {
                broadcast
                    .content(
                        "You must have started the game or be an owner of the bot to make \
                         others dead\nTo make yourself dead, please use the reactions",
                    )?
                    .await?;
            }
        } else {
            self.discord_http
                .create_message(msg.channel_id)
                .content("There is no game running")?
                .await?;
        }

        Ok(())
    }

    async fn make_dead(&self, target: UserId) {
        let mut fut = None;

        if let Some(game) = self.game.write().as_mut() {
            if game.dead.insert(target) && game.meeting_in_progress {
                let guild_id = game.guild_id;
                fut = Some(
                    self.discord_http
                        .update_guild_member(guild_id, target)
                        .mute(true),
                );
            }
        }

        if let Some(fut) = fut {
            if let Err(why) = fut.await {
                error!("Error occured when making {} dead\n{}", target, why);
            }
        }
    }

    async fn batch<F, O>(&self, futs: Vec<F>)
    where
        F: Future<Output = TwiResult<O>>,
    {
        let errors = futures::future::join_all(futs)
            .await
            .into_iter()
            .filter_map(StdResult::err)
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            let channel = self.game.read().as_ref().map(|g| g.ctrl_channel);
            if let Some(channel) = channel {
                let _ = self
                    .discord_http
                    .create_message(channel)
                    .content("errors occurred; check log")
                    .unwrap()
                    .await;
            }
            for error in errors {
                error!("{}", error);
            }
        }
    }

    fn broadcast(&self) -> Option<CreateMessage<'_>> {
        self.game
            .read()
            .as_ref()
            .map(|g| self.discord_http.create_message(g.ctrl_channel))
    }

    fn get_members_in_channel(&self, voice_channel: &GuildChannel) -> Vec<Arc<CachedMember>> {
        self.cache
            .voice_channel_states(voice_channel.id())
            .map_or(Vec::new(), |vs| {
                vs.iter()
                    .map(|vs| self.cache.member(vs.guild_id.unwrap(), vs.user_id).unwrap())
                    .filter(|m| !m.user.bot && !m.roles.contains(&self.config.spectator_role))
                    .collect()
            })
    }

    fn is_game_in_progress(&self) -> bool {
        self.game.read().is_some()
    }

    fn is_in_control(&self, user_id: UserId) -> bool {
        matches!(self.owners.read().as_ref(), Some(owners) if owners.contains(&user_id))
            || matches!(self.game.read().as_ref(), Some(game) if game.ctrl_user == user_id)
    }

    fn is_reacting_to_control(&self, reaction: &Reaction) -> bool {
        matches!(
            self.game.read().as_ref(),
            Some(game) if game.ctrl_msg == reaction.message_id,
        )
    }
}
