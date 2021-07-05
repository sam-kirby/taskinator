use twilight_cache_inmemory::model::CachedMember;
use twilight_http::{request::prelude::CreateMessage, Client};
use twilight_model::{channel::Message, user::User};

use crate::Result;

pub trait KnownAs {
    fn known_as(&self) -> String;
}

impl KnownAs for (&CachedMember, User) {
    fn known_as(&self) -> String {
        self.0
            .nick
            .as_ref()
            .map_or_else(|| self.1.name.clone(), Clone::clone)
    }
}

pub trait ReplyTo {
    fn reply<'a>(
        &self,
        client: &'a Client,
        content: impl Into<String>,
    ) -> Result<CreateMessage<'a>>;
}

impl ReplyTo for Message {
    fn reply<'a>(
        &self,
        client: &'a Client,
        content: impl Into<String>,
    ) -> Result<CreateMessage<'a>> {
        Ok(client
            .create_message(self.channel_id)
            .reply(self.id)
            .content(content)?)
    }
}
