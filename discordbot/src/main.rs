mod app;
mod commands;
mod config;
mod create;
mod events;
mod interact;
mod masters;
mod ops;
mod pubsub;
mod render;
mod sheets;
mod store;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client;
use twilight_model::gateway::event::Event;

use crate::app::{App, Data};
use crate::config::{Config, Env};
use crate::masters::Masters;
use crate::sheets::Sheets;

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,twilight_gateway=warn,twilight_http=warn".into());
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    let env = Env::load()?;
    let cfg = Config::load(&env.config_path)?;
    let sheets = Sheets::new(&env.google_sa_json, env.spreadsheet_id.clone()).await?;
    let masters = Masters::load(&sheets, &cfg)
        .await
        .context("masters シートの読み込みに失敗しました")?;

    let http = Client::new(env.discord_token.clone());
    let application_id = http
        .current_user_application()
        .await
        .context("Bot の情報を取得できません (DISCORD_TOKEN を確認してください)")?
        .model()
        .await?
        .id;
    let app: Data = Arc::new(App::new(
        env.clone(),
        cfg,
        sheets,
        masters,
        http,
        application_id,
    ));
    setup(&app).await?;
    if let Some(sub) = app.cfg.sync.pubsub_subscription.clone() {
        pubsub::spawn(app.clone(), sub);
    }

    let intents = Intents::GUILDS
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT
        | Intents::GUILD_MEMBERS;
    let wanted = EventTypeFlags::READY
        | EventTypeFlags::INTERACTION_CREATE
        | EventTypeFlags::MESSAGE_CREATE
        | EventTypeFlags::MEMBER_ADD;
    let mut shard = Shard::new(ShardId::ONE, env.discord_token, intents);
    tracing::info!("TTM-DB 起動");
    // READY は新規セッション確立のたびに届く。初回は起動完了、以降は再接続として通知
    let mut announced = false;
    while let Some(item) = shard.next_event(wanted).await {
        match item {
            Ok(event) => {
                if matches!(event, Event::Ready(_)) {
                    let msg = if announced {
                        "🔄 TTM-DB: Discord に再接続しました。"
                    } else {
                        "🟢 TTM-DB: 起動が完了しました。"
                    };
                    announced = true;
                    let app = app.clone();
                    tokio::spawn(async move { ops::alert(&app, msg.to_owned()).await });
                }
                tokio::spawn(events::handle(app.clone(), event));
            }
            Err(e) => tracing::warn!(error = %e, "Gateway イベントの受信に失敗"),
        }
    }
    Ok(())
}

/// コマンド登録・起動時バリデーション・タグ同期
async fn setup(app: &App) -> Result<()> {
    app.interaction()
        .set_guild_commands(app.guild_id(), &commands::definitions())
        .await
        .context("コマンドの登録に失敗しました")?;
    if let Err(e) = ops::load_checked(app).await {
        tracing::error!(error = %e, "tickets シートの読み込みに失敗");
    }
    match render::sync_forum_tags(app, false).await {
        Ok(r) if !r.is_empty() => tracing::info!(changes = ?r, "Forum タグを同期"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "Forum タグの同期に失敗"),
    }
    if app.cfg.sync.pubsub_subscription.is_none() {
        tracing::warn!(
            "sync.pubsub_subscription が未設定のため、スプシ側の編集は Discord に反映されません"
        );
    }
    Ok(())
}
