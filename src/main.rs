#![deny(
    clippy::all,
    clippy::pedantic,
    future_incompatible,
    nonstandard_style,
    rust_2018_idioms,
    unused,
    warnings
)]

mod bot;
mod config;
mod constants;

use crate::bot::Bot;

use tokio_stream::StreamExt;
use tracing::error;
use twilight_model::gateway::event::Event;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

fn main() -> Result<()> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { bot_main().await })
}

async fn bot_main() -> Result<()> {
    // Setup
    tracing_subscriber::fmt::init();

    let mut bot = Bot::new("./Config.toml")?;

    bot.fetch_app_details().await?;

    // Start gateway
    let mut events = bot.start_gateway().await?;

    // Gateway event loop
    while let Some(event) = events.next().await {
        bot.update_cache(&event);

        match event {
            Event::MessageCreate(message) if !message.author.bot => {
                let bot_clone = bot.clone();
                tokio::spawn(async move {
                    if let Err(e) = bot_clone.process_command(&message).await {
                        error!(
                            "Error processing command\nMessage: {:?}\nError: {}",
                            &message, e
                        );
                    }
                });
            }
            Event::ReactionAdd(reaction) if reaction.user_id != bot.bot_id() => {
                let bot_clone = bot.clone();
                tokio::spawn(async move {
                    if let Err(e) = bot_clone.reaction_add_handler(&reaction).await {
                        error!("Error handling reaction add: {}", e);
                    }
                });
            }
            Event::ReactionRemove(reaction) if reaction.user_id != bot.bot_id() => {
                let bot_clone = bot.clone();
                tokio::spawn(async move {
                    if let Err(e) = bot_clone.reaction_remove_handler(&reaction).await {
                        error!("Error handling reaction remove: {}", e);
                    }
                });
            }
            _ => {}
        }
    }

    Ok(())
}
