use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use parking_lot::RwLock;
use taskinator_communicator::game::{MeetingState, Player, State};
use tokio::{signal::ctrl_c, sync::watch::Receiver, time::sleep};
use twilight_cache_inmemory::{model::CachedMember, InMemoryCache, ResourceType};
use twilight_command_parser::{Arguments, Command, CommandParserConfig, Parser};
use twilight_embed_builder::{EmbedBuilder, EmbedFieldBuilder};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard};
use twilight_http::{Client, Result as TwiResult};
use twilight_mention::{Mention, ParseMention};
use twilight_model::{
    channel::{Channel, GuildChannel, Message},
    id::{ChannelId, UserId},
};

use crate::{
    config::Config,
    utils::{KnownAs, ReplyTo},
    Result,
};

enum BotState {
    PreGame,
    InGame,
    InMeeting,
    GameOver,
}

pub struct Builder {
    cache: InMemoryCache,
    discord_gateway: Shard,
    discord_client: Client,
    command_parser: Arc<Parser<'static>>,
    broadcast_channel: ChannelId,
    living_channel: ChannelId,
    dead_channel: ChannelId,
}

impl Builder {
    pub async fn build(self, game_state_rx: Receiver<Option<State>>) -> Result<Bot> {
        let (owners, bot_id) = {
            let mut owners = HashSet::new();

            let app_info = self.discord_client.current_user_application().await?;
            if let Some(team) = app_info.team {
                owners.extend(team.members.iter().map(|tm| tm.user.id));
            } else {
                owners.insert(app_info.owner.id);
            }
            (Arc::new(owners), UserId(app_info.id.0))
        };

        // Validate channels
        let broadcast_channel = if let Channel::Guild(channel) = self
            .discord_client
            .channel(self.broadcast_channel)
            .await?
            .expect("Failed to retreive the broadcast channel")
        {
            if let GuildChannel::Text(tc) = channel {
                tc
            } else {
                tracing::error!("Broadcast channel must be a text channel");
                panic!();
            }
        } else {
            tracing::error!("Broadcast channel must be in a guild.");
            panic!();
        };

        let living_channel = if let Channel::Guild(channel) = self
            .discord_client
            .channel(self.living_channel)
            .await?
            .expect("Failed to retreive the living channel")
        {
            if let GuildChannel::Voice(vc) = channel {
                vc
            } else {
                tracing::error!("Living channel must be a voice channel");
                panic!();
            }
        } else {
            tracing::error!("Living channel must be in a guild.");
            panic!();
        };

        let dead_channel = if let Channel::Guild(channel) = self
            .discord_client
            .channel(self.dead_channel)
            .await?
            .expect("Failed to retreive the dead channel")
        {
            if let GuildChannel::Voice(vc) = channel {
                vc
            } else {
                tracing::error!("Dead channel must be a voice channel");
                panic!();
            }
        } else {
            tracing::error!("Dead channel must be in a guild.");
            panic!();
        };

        Ok(Bot {
            cache: self.cache,
            discord_gateway: self.discord_gateway,
            discord_client: self.discord_client,
            command_parser: self.command_parser,
            bot_id,
            owners,
            broadcast_channel: broadcast_channel.id,
            living_channel: living_channel.id,
            dead_channel: dead_channel.id,
            player_names: Arc::new(RwLock::new(HashMap::new())),
            game_state_rx,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Bot {
    cache: InMemoryCache,
    discord_gateway: Shard,
    discord_client: Client,
    command_parser: Arc<Parser<'static>>,
    bot_id: UserId,
    owners: Arc<HashSet<UserId>>,
    broadcast_channel: ChannelId,
    living_channel: ChannelId,
    dead_channel: ChannelId,
    player_names: Arc<RwLock<HashMap<UserId, String>>>,
    game_state_rx: Receiver<Option<State>>,
}

impl Bot {
    pub fn builder(config_path: impl AsRef<Path>) -> Builder {
        let config = match Config::from_file(config_path) {
            Ok(config) => config,
            Err(why) => {
                tracing::error!("Failed to read the config file. Aborting!");
                tracing::error!("{}", why);
                panic!();
            }
        };

        let discord_client = Client::new(&config.token);

        let discord_gateway = Shard::new(
            &config.token,
            Intents::GUILDS
                | Intents::GUILD_MEMBERS
                | Intents::GUILD_MESSAGES
                | Intents::GUILD_VOICE_STATES,
        );

        let (broadcast_channel, living_channel, dead_channel) = (
            config.broadcast_channel,
            config.living_channel,
            config.dead_channel,
        );

        let cache = InMemoryCache::builder()
            .resource_types(
                ResourceType::CHANNEL
                    | ResourceType::GUILD
                    | ResourceType::MEMBER
                    | ResourceType::USER
                    | ResourceType::VOICE_STATE,
            )
            .build();

        let command_parser = {
            let mut parser_config = CommandParserConfig::new();
            parser_config.add_prefix("~");
            parser_config.add_command("ident", false);
            parser_config.add_command("check", false);
            parser_config.add_command("stop", false);

            Arc::new(Parser::new(parser_config))
        };

        Builder {
            cache,
            discord_gateway,
            discord_client,
            command_parser,
            broadcast_channel,
            living_channel,
            dead_channel,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let shutdown_handle = self.discord_gateway.clone();

        tokio::spawn(async move {
            if let Err(why) = ctrl_c().await {
                tracing::error!("There was an error registering the ctrl+c handler");
                tracing::error!("{}", why);
            }

            shutdown_handle.shutdown();
        });

        self.discord_gateway.start().await?;

        let event_flags: EventTypeFlags = EventTypeFlags::GUILD_CREATE
            | EventTypeFlags::MEMBER_ADD
            | EventTypeFlags::MEMBER_UPDATE
            | EventTypeFlags::MESSAGE_CREATE
            | EventTypeFlags::VOICE_STATE_UPDATE;

        let mut events = self.discord_gateway.some_events(event_flags);

        let mut bot = self.clone();
        tokio::spawn(async move {
            let mut bot_state = BotState::PreGame;
            loop {
                if let Err(why) = bot.game_state_rx.changed().await {
                    tracing::error!("Game state receive failed: {}", why);
                    break;
                }
                let state = bot.game_state_rx.borrow().as_ref().map(|s| (*s).clone());
                match state {
                    Some(State::InGame { meeting, .. })
                        if matches!(
                            meeting,
                            MeetingState::Discussion
                                | MeetingState::NotVoted
                                | MeetingState::Voted
                                | MeetingState::Results
                        ) =>
                    {
                        // In a meeting
                        if matches!(bot_state, BotState::PreGame | BotState::InGame) {
                            bot_state = BotState::InMeeting;
                            bot.start_meeting().await;
                        }
                    }
                    Some(State::InGame { .. }) => {
                        // In gameplay
                        match bot_state {
                            BotState::InMeeting => {
                                bot.end_meeting(&mut bot_state).await;
                            }
                            BotState::PreGame => {
                                bot_state = BotState::InGame;
                                bot.start_game().await;
                            }
                            _ => {}
                        }
                    }
                    Some(State::Lobby { .. }) | Some(State::Menu) | None => {
                        // No game running or crash
                        if matches!(bot_state, BotState::InGame | BotState::InMeeting) {
                            bot_state = BotState::PreGame;
                            bot.end_game().await;
                        }

                        if matches!(bot_state, BotState::GameOver) {
                            bot_state = BotState::PreGame;
                        }
                    }
                }
            }
        });

        while let Some(event) = events.next().await {
            self.cache.update(&event);

            match event {
                Event::MessageCreate(message) if !message.author.bot => {
                    if let Err(why) = self.handle_command(&message).await {
                        tracing::error!("An error occurred whilst processing a command!");
                        tracing::error!("Message: {:?}", &message);
                        tracing::error!("Error: {}", why);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_command(&self, message: &Message) -> Result<()> {
        match self.command_parser.parse(&message.content) {
            Some(Command {
                name: "ident",
                mut arguments,
                ..
            }) => self.ident_player(&message, &mut arguments).await?,
            Some(Command { name: "check", .. }) => self.check_matching(&message).await?,
            Some(Command { name: "stop", .. }) => {
                if self.owners.contains(&message.author.id) {
                    self.discord_client
                        .create_message(message.channel_id)
                        .content("Good night")?
                        .reply(message.id)
                        .await?;
                    self.discord_gateway.shutdown();
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn start_meeting(&self) {
        tracing::info!("Start meeting");

        let mut futs = self
            .match_members_to_players(&self.get_members_in_channel(self.living_channel))
            .expect("failed to match players at start of meeting - this should not happen!")
            .iter()
            .filter_map(|(m, p)| match p {
                Some(p) if !p.dead => Some(
                    self.discord_client
                        .update_guild_member(m.guild_id, m.user.id)
                        .mute(false),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();

        futs.extend(
            self.get_members_in_channel(self.dead_channel)
                .iter()
                .map(|m| {
                    self.discord_client
                        .update_guild_member(m.guild_id, m.user.id)
                        .channel_id(self.living_channel)
                        .mute(true)
                }),
        );

        self.batch(futs).await;
    }

    async fn end_meeting(&self, bot_state: &mut BotState) {
        tracing::info!("End meeting");

        sleep(Duration::from_secs(10)).await;

        let game_over = {
            let state = self.game_state_rx.borrow();

            match &*state {
                Some(State::InGame { players, .. }) => {
                    let (imposters, crew) = players
                        .iter()
                        .filter(|p| !p.dead)
                        .partition::<Vec<_>, _>(|p| p.impostor);

                    imposters.is_empty() || imposters.len() >= crew.len()
                }
                _ => true,
            }
        };

        if game_over {
            tracing::info!("Game is, in fact, over");
            *bot_state = BotState::GameOver;
            return self.end_game().await;
        }

        *bot_state = BotState::InGame;

        self.mute_players().await;
    }

    async fn start_game(&self) {
        tracing::info!("START GAME!");

        self.mute_players().await;
    }

    async fn end_game(&self) {
        tracing::info!("End game");

        let mut futs = self
            .get_members_in_channel(self.living_channel)
            .iter()
            .map(|m| {
                self.discord_client
                    .update_guild_member(m.guild_id, m.user.id)
                    .mute(false)
            })
            .collect::<Vec<_>>();

        futs.extend(
            self.get_members_in_channel(self.dead_channel)
                .iter()
                .map(|m| {
                    self.discord_client
                        .update_guild_member(m.guild_id, m.user.id)
                        .channel_id(self.living_channel)
                }),
        );

        self.batch(futs).await;
    }

    async fn mute_players(&self) {
        let futs = self
            .match_members_to_players(&self.get_members_in_channel(self.living_channel))
            .expect("failed to match players at end of meeting - this should not happen!")
            .iter()
            .filter_map(|(m, p)| match p {
                Some(p) if p.dead => Some(
                    self.discord_client
                        .update_guild_member(m.guild_id, m.user.id)
                        .channel_id(self.dead_channel)
                        .mute(false),
                ),
                Some(p) if !p.dead => Some(
                    self.discord_client
                        .update_guild_member(m.guild_id, m.user.id)
                        .mute(true),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();

        self.batch(futs).await;
    }

    async fn ident_player(&self, message: &Message, arguments: &mut Arguments<'_>) -> Result<()> {
        match arguments.next() {
            Some(argument) => {
                if let Ok(target) = UserId::parse(argument) {
                    if self.owners.contains(&message.author.id) {
                        if let Some(ign) = arguments.next() {
                            self.player_names.write().insert(target, ign.to_owned());
                            message
                                .reply(
                                    &self.discord_client,
                                    format!("Set {}'s IGN to {}", target.mention(), ign),
                                )?
                                .await?;
                        } else {
                            message
                                .reply(&self.discord_client, "You must include an in game name")?
                                .await?;
                        }
                    } else {
                        message
                            .reply(
                                &self.discord_client,
                                "Only owners can set the in game name of another user",
                            )?
                            .await?;
                    }
                } else {
                    self.player_names
                        .write()
                        .insert(message.author.id, argument.to_owned());
                    message
                        .reply(
                            &self.discord_client,
                            format!("Set your in game name to {}", argument),
                        )?
                        .await?;
                }
            }
            None => {
                message
                    .reply(&self.discord_client, "Please include your in game name")?
                    .await?;
            }
        };

        Ok(())
    }

    async fn check_matching(&self, message: &Message) -> Result<()> {
        match self.match_members_to_players(&self.get_members_in_channel(self.living_channel)) {
            Some(matched_players) => {
                tracing::trace!("{:?}", matched_players);
                let unmatched_players = matched_players
                    .into_iter()
                    .filter_map(|(m, p)| if p.is_none() { Some(m.user.id) } else { None })
                    .collect::<Vec<_>>();

                if unmatched_players.is_empty() {
                    self.discord_client
                        .create_message(message.channel_id)
                        .content("All members matched to player")?
                        .reply(message.id)
                        .await?;
                } else {
                    let embed = EmbedBuilder::new()
                        .description("Could not match all members to players")?
                        .color(0xFF_00_00)?;

                    let embed = unmatched_players.iter().fold(embed, |embed, uid| {
                        embed.field(
                            EmbedFieldBuilder::new("not found", format!("{}", uid.mention()))
                                .unwrap()
                                .build(),
                        )
                    });

                    self.discord_client
                        .create_message(message.channel_id)
                        .embed(embed.build()?)?
                        .await?;
                }
            }
            None => {
                self.discord_client
                    .create_message(message.channel_id)
                    .content("Must be in a lobby to check")?
                    .reply(message.id)
                    .await?;
            }
        };

        Ok(())
    }

    fn match_members_to_players(
        &self,
        members: &[Arc<CachedMember>],
    ) -> Option<Vec<(Arc<CachedMember>, Option<Player>)>> {
        let game_state = self.game_state_rx.borrow();
        let players = match &*game_state {
            Some(State::Lobby { players }) | Some(State::InGame { players, .. }) => Some(players),
            Some(_) | None => None,
        };

        players.map(|players| {
            members
                .iter()
                .map(|m| {
                    let ign = match self.player_names.read().get(&m.user.id) {
                        Some(ign) => ign.to_owned(),
                        None => m.known_as(),
                    };
                    (
                        m.clone(),
                        players.iter().find_map(
                            |p| {
                                if p.name == ign {
                                    Some(p.clone())
                                } else {
                                    None
                                }
                            },
                        ),
                    )
                })
                .collect()
        })
    }

    fn get_members_in_channel(&self, channel: ChannelId) -> Vec<Arc<CachedMember>> {
        self.cache
            .voice_channel_states(channel)
            .map_or(Vec::new(), |vs| {
                vs.iter()
                    .map(|vs| self.cache.member(vs.guild_id.unwrap(), vs.user_id).unwrap())
                    .filter(|m| !m.user.bot)
                    .collect()
            })
    }

    async fn batch<Fut, Out>(&self, futs: Vec<Fut>) -> Vec<Out>
    where
        Fut: Future<Output = TwiResult<Out>>,
        Out: Debug,
    {
        let (successes, errors) = futures::future::join_all(futs)
            .await
            .into_iter()
            .partition::<Vec<_>, _>(TwiResult::is_ok);

        let errors = errors
            .into_iter()
            .map(TwiResult::unwrap_err)
            .collect::<Vec<_>>();

        if !errors.is_empty() {
            let _ = self
                .discord_client
                .create_message(self.broadcast_channel)
                .content("Errors occurred during batch operation, check logs")
                .unwrap()
                .await;
            for error in errors {
                tracing::warn!("{}", error);
            }
        }

        successes.into_iter().map(TwiResult::unwrap).collect()
    }
}
