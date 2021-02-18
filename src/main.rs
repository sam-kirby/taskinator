#![deny(
    clippy::all,
    clippy::pedantic,
    future_incompatible,
    nonstandard_style,
    rust_2018_idioms,
    warnings
)]

mod bot;
mod config;
mod utils;

use std::time::Duration;

use crate::bot::Builder;

use sysinfo::{ProcessExt, RefreshKind, System, SystemExt};
use taskinator_communicator::game::Game;
use tokio::{runtime, sync::watch, task::JoinHandle, time::sleep};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

fn main() -> Result<()> {
    runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("taskinator-worker")
        .enable_all()
        .build()?
        .block_on(async { bot_main().await })
}

async fn bot_main() -> Result<()> {
    // Setup
    tracing_subscriber::fmt()
        .with_env_filter("taskinator=info,taskinator_communicator=info,warn")
        .init();

    // Start Among Us watcher task
    let (tx, rx) = watch::channel(None);
    let _au_watcher: JoinHandle<Result<()>> = tokio::spawn(async move {
        const CONN_RETRY_DELAY: u64 = 5;
        const POLLING_DELAY: u64 = 3;
        const MAX_CONSEC_FAILS: u64 = 3;

        let among_us_pid = {
            let mut system = System::new_with_specifics(RefreshKind::new().with_processes());

            loop {
                system.refresh_processes();

                if let Some(among_us_proc) = system.get_process_by_name("Among Us.exe").first() {
                    break among_us_proc.pid();
                }

                tracing::warn!("Could not find Among Us process... That's a bit sus.");
                tracing::warn!("Will retry in {} seconds", CONN_RETRY_DELAY);

                sleep(Duration::from_secs(CONN_RETRY_DELAY)).await;
            }
        };

        tracing::info!("Among Us process found! PID: {}", among_us_pid);

        let among_us = match Game::from_pid(among_us_pid) {
            Ok(game) => game,
            Err(why) => {
                tracing::error!(
                    "Opening a connection to the game failed, \
                                make sure you have sufficient permissions"
                );
                return Err(why);
            }
        };

        tracing::info!("Established connection to Among Us");

        let mut failure_count = 0;
        loop {
            match among_us.state() {
                Ok(state) => {
                    tracing::debug!("Read updated state!");
                    tracing::trace!("{:?}", state);
                    failure_count = 0;
                    tx.send(Some(state))?;
                }
                Err(why) => {
                    if failure_count < MAX_CONSEC_FAILS {
                        // If failure count has not reached max, increment but DO NOT update the
                        // channel
                        failure_count += 1;
                        tracing::warn!(
                            "An error occurred reading Among Us' state ({}/{}). \
                            This can happen when the game is starting or changing level.",
                            failure_count,
                            MAX_CONSEC_FAILS,
                        );
                        tracing::warn!("{}", why);
                    } else {
                        // At max failure count, log an error and set the channel to None to signal
                        // no running game
                        tracing::error!(
                            "Failed to read Among Us' state again. \
                            Retries exhausted, has the game closed?"
                        );
                        tracing::error!("{}", why);
                        tx.send(None)?;
                        return Err(why);
                    }
                }
            }
            sleep(Duration::from_secs(POLLING_DELAY)).await;
        }
    });

    // Setup bot
    tracing::info!("Constructing bot instance from config");
    let mut bot = Builder::new("./Config.toml").build(rx).await?;

    bot.start().await?;

    Ok(())
}
