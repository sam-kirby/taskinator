use twilight_cache_inmemory::model::CachedMember;
use twilight_http::{request::prelude::CreateMessage, Client};
use twilight_model::channel::Message;

use crate::Result;

pub trait KnownAs {
    fn known_as(&self) -> String;
}

impl KnownAs for CachedMember {
    fn known_as(&self) -> String {
        self.nick
            .as_ref()
            .map_or_else(|| self.user.name.to_owned(), ToOwned::to_owned)
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
