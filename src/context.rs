use std::{collections::HashSet, result::Result as StdResult, sync::Arc};

use crate::{config::Config, Result};

use futures::Future;
use tokio::sync::RwLock;
use tracing::error;
use twilight_cache_inmemory::{model::CachedMember, InMemoryCache as DiscordCache};
use twilight_gateway::Shard;
use twilight_http::{
    error::Result as TwiResult, request::channel::message::CreateMessage, Client as DiscordHttp,
};
use twilight_model::{
    channel::GuildChannel,
    channel::{Message, Reaction},
    id::{ChannelId, GuildId, MessageId, UserId},
};

#[derive(Clone)]
pub struct Context {
    pub config: Arc<Config>,
    pub discord_http: DiscordHttp,
    pub cache: DiscordCache,
    pub shard: Shard,
    pub owners: Arc<HashSet<UserId>>,
    game: Arc<RwLock<Option<Game>>>,
}

impl Context {
    pub fn new(
        config: Config,
        discord_http: DiscordHttp,
        cache: DiscordCache,
        shard: Shard,
        owners: HashSet<UserId>,
    ) -> Self {
        Context {
            config: Arc::new(config),
            discord_http,
            cache,
            shard,
            owners: Arc::new(owners),
            game: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn batch<F, O>(&self, futs: Vec<F>)
    where
        F: Future<Output = TwiResult<O>>,
    {
        let errors = futures::future::join_all(futs)
            .await
            .into_iter()
            .filter_map(StdResult::err)
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            if let Some(channel) = self.game.read().await.as_ref().map(|g| g.ctrl_channel) {
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

    pub async fn broadcast(&self) -> Option<CreateMessage<'_>> {
        if let Some(game) = self.game.read().await.as_ref() {
            Some(self.discord_http.create_message(game.ctrl_channel))
        } else {
            None
        }
    }

    pub async fn make_dead(&mut self, target: &UserId) {
        if let Some(game) = self.game.write().await.as_mut() {
            if game.dead.insert(*target) && game.meeting_in_progress {
                if let Err(why) = self
                    .discord_http
                    .update_guild_member(game.guild_id, *target)
                    .mute(true)
                    .await
                {
                    error!("Error occurred when making {} dead:\n{}", target, why);
                }
            }
        }
    }

    pub async fn is_in_control(&self, user_id: &UserId) -> bool {
        self.owners.contains(&user_id)
            || self
                .game
                .read()
                .await
                .as_ref()
                .map_or(false, |g| g.ctrl_user == *user_id)
    }

    pub async fn is_reacting_to_control(&self, reaction: &Reaction) -> bool {
        self.game
            .read()
            .await
            .as_ref()
            .map_or(false, |g| g.ctrl_msg == reaction.message_id)
    }

    pub async fn start_game(&mut self, msg: &Message, guild_id: GuildId) {
        self.game.write().await.replace(Game {
            dead: HashSet::new(),
            ctrl_channel: msg.channel_id,
            ctrl_msg: msg.id,
            ctrl_user: msg.author.id,
            guild_id,
            meeting_in_progress: false,
        });
    }

    pub async fn is_game_in_progress(&self) -> bool {
        self.game.read().await.is_some()
    }

    pub async fn get_members_in_channel(
        &self,
        voice_channel: Arc<GuildChannel>,
    ) -> Vec<Arc<CachedMember>> {
        match self.cache.voice_channel_states(voice_channel.id()) {
            Some(vs) => vs
                .iter()
                .map(|vs| self.cache.member(vs.guild_id.unwrap(), vs.user_id).unwrap())
                .filter(|m| !m.user.bot && !m.roles.contains(&self.config.spectator_role))
                .collect(),
            None => Vec::new(),
        }
    }

    pub async fn mute_players(&self) -> Result<()> {
        let living_channel = self
            .cache
            .guild_channel(self.config.living_channel)
            .unwrap();

        let (alive_players, dead_players): (Vec<_>, Vec<_>) = {
            let game_lock = self.game.read().await;
            let game = game_lock.as_ref().unwrap();

            self.get_members_in_channel(living_channel)
                .await
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

        let mut game_lock = self.game.write().await;
        let g = game_lock.as_mut().expect("expected game");
        g.meeting_in_progress = false;

        Ok(())
    }

    pub async fn emergency_meeting(&self) -> Result<()> {
        let living_channel = self
            .cache
            .guild_channel(self.config.living_channel)
            .unwrap();
        let dead_channel = self.cache.guild_channel(self.config.dead_channel).unwrap();

        let mut futures = Vec::new();

        {
            let game_lock = self.game.read().await;
            let game = game_lock.as_ref().unwrap();

            for member in self.get_members_in_channel(living_channel).await {
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

        for member in self.get_members_in_channel(dead_channel).await {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .channel_id(self.config.living_channel)
                    .mute(true),
            )
        }

        self.batch(futures).await;

        let mut game_lock = self.game.write().await;
        let g = game_lock.as_mut().expect("expected game");
        g.meeting_in_progress = true;

        Ok(())
    }

    pub async fn end_game(&mut self) -> Result<()> {
        if let Some(game) = self.game.write().await.take() {
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

        for member in self.get_members_in_channel(living_channel).await {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .mute(false),
            );
        }

        for member in self.get_members_in_channel(dead_channel).await {
            futures.push(
                self.discord_http
                    .update_guild_member(member.guild_id, member.user.id)
                    .channel_id(self.config.living_channel),
            );
        }

        self.batch(futures).await;

        Ok(())
    }
}

struct Game {
    dead: HashSet<UserId>,
    ctrl_channel: ChannelId,
    ctrl_msg: MessageId,
    ctrl_user: UserId,
    guild_id: GuildId,
    meeting_in_progress: bool,
}
