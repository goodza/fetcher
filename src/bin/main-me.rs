use std::time::Duration;

use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    std::env::set_var("RUST_LOG", "info");
    let token = std::env::var("TG_TOKEN").expect("TG_TOKEN is not set");
    pretty_env_logger::init();
    log::info!("Starting");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to create client");
    let bot = Bot::with_client(token, client);
    teloxide::repl(bot, handle_msg).await;
}

async fn handle_msg(_msg: Message, _bot: Bot) -> ResponseResult<()> {
    Ok(())
}

