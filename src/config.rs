use crate::Result;

use std::{fs::File, io::Read, path::Path};

use serde::Deserialize;
use twilight_model::id::{ChannelId, RoleId};

#[derive(Deserialize)]
pub struct Config {
    pub token: String,
    pub living_channel: ChannelId,
    pub dead_channel: ChannelId,
    pub spectator_role: RoleId,
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = File::open(path.as_ref())?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;

        let config_str = String::from_utf8(contents)?;

        let config: Config = toml::from_str(&config_str)?;

        Ok(config)
    }
}
