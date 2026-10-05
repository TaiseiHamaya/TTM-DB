//! Gateway イベントの振り分け: コマンド・フォーム・ボタン、画像添付検知、GAS からの同期データ、参加者追加

use anyhow::Result;
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::Message;
use twilight_model::gateway::event::Event;

use crate::app::{Data, display_name, is_admin};
use crate::interact::{self, defer_ephemeral, edit_reply, respond};
use crate::masters::StatusRole;
use crate::{commands, create, ops, render};

const OK: RequestReactionType<'static> = RequestReactionType::Unicode { name: "✅" };
const NG: RequestReactionType<'static> = RequestReactionType::Unicode { name: "⚠" };

pub async fn handle(app: Data, event: Event) {
    match event {
        Event::InteractionCreate(i) => {
            let i = &i.0;
            if i.guild_id != Some(app.guild_id()) {
                return;
            }
            if let Err(e) = on_interaction(&app, i).await {
                tracing::error!(error = %e, "インタラクションの処理に失敗");
                interact::report_error(&app, i, &e).await;
            }
        }
        Event::MessageCreate(m) => on_message(&app, &m.0).await,
        Event::MemberAdd(m) if m.guild_id == app.guild_id() && !m.member.user.bot => {
            let u = &m.member.user;
            let name = app
                .sheet_name_of(u.id.get(), &u.name, display_name(m.member.nick.as_deref(), u))
                .await;
            if let Err(e) = ops::add_member(&app, u.id.get(), &name).await {
                tracing::warn!(error = %e, "参加者の masters 追加に失敗");
            }
        }
        Event::Ready(r) => tracing::info!(user = %r.user.name, "Gateway に接続"),
        _ => {}
    }
}

async fn on_interaction(app: &Data, i: &Interaction) -> Result<()> {
    match (i.kind, &i.data) {
        (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(d))) => {
            commands::on_command(app, i, d).await
        }
        (
            InteractionType::ApplicationCommandAutocomplete,
            Some(InteractionData::ApplicationCommand(d)),
        ) => commands::on_autocomplete(app, i, d).await,
        (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(d))) => {
            if let Some(rest) = d.custom_id.strip_prefix(render::BUTTON_PREFIX) {
                on_status_button(app, i, rest).await
            } else if let Some(rest) = d
                .custom_id
                .strip_prefix(create::PREFIX)
                .and_then(|s| s.strip_prefix(':'))
            {
                create::on_component(app, i, d, rest).await
            } else {
                Ok(())
            }
        }
        (InteractionType::ModalSubmit, Some(InteractionData::ModalSubmit(d)))
            if d.custom_id.starts_with(create::PREFIX) =>
        {
            create::on_modal(app, i, d).await
        }
        _ => Ok(()),
    }
}

/// チケット先頭メッセージの状態変更ボタン
async fn on_status_button(app: &Data, i: &Interaction, rest: &str) -> Result<()> {
    // custom_id = ttm:st:{ticket_id}:{role}
    let Some((ticket_id, role)) = rest.rsplit_once(':') else {
        return Ok(());
    };
    let Some(role) = StatusRole::from_key(role) else {
        return Ok(());
    };
    let Some(actor) = i.author_id() else {
        return Ok(());
    };
    respond(app, i, &defer_ephemeral()).await?;
    let admin = is_admin(i.member.as_ref().and_then(|m| m.permissions));
    let seen = i.message.as_ref().and_then(render::updated_at_from_embed);
    let text = match ops::change_status(app, ticket_id, role, actor, admin, seen).await {
        Ok(ops::StatusOutcome::Changed { from, to }) => {
            format!("「{from}」→「{to}」に変更しました")
        }
        Err(e) => format!("変更できませんでした: {e:#}"),
    };
    edit_reply(app, i, &text).await
}

/// スプシの GAS が Webhook で投稿した同期データ (添付 JSON) を Discord に反映する
async fn on_sheet_push(app: &Data, msg: &Message) {
    let result = async {
        let file = msg
            .attachments
            .iter()
            .find(|a| a.filename.ends_with(".json"))
            .ok_or_else(|| anyhow::anyhow!("同期データの JSON が添付されていません"))?;
        let bytes = app
            .web
            .get(&file.url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let snap: ops::Snapshot = serde_json::from_slice(&bytes)?;
        ops::apply_snapshot(app, snap).await
    }
    .await;
    let reaction = match result {
        Ok(n) => {
            tracing::info!(count = n, message_id = msg.id.get(), "スプシからの同期データを反映");
            &OK
        }
        Err(e) => {
            tracing::error!(error = %e, message_id = msg.id.get(), "スプシからの同期データの反映に失敗");
            &NG
        }
    };
    let _ = app.http.create_reaction(msg.channel_id, msg.id, reaction).await;
}

/// Forum Post 内に画像が投稿されたら image_urls に追記する。
/// GAS の Webhook 投稿なら同期データとして処理する
async fn on_message(app: &Data, msg: &Message) {
    if let (Some(hook), Some(expected)) = (msg.webhook_id, app.cfg.discord.sync_webhook_id)
        && hook.get() == expected
    {
        on_sheet_push(app, msg).await;
        return;
    }
    if msg.author.bot || msg.guild_id != Some(app.guild_id()) {
        return;
    }
    let urls: Vec<String> = msg
        .attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        })
        .map(|a| a.url.clone())
        .collect();
    if urls.is_empty() {
        return;
    }
    // Forum 配下のスレッドか確認 (チケット以外のチャンネルでは Sheets を読まない)
    let parent = match app.http.channel(msg.channel_id).await {
        Ok(res) => res.model().await.ok().and_then(|ch| ch.parent_id),
        Err(_) => None,
    };
    if parent != Some(app.forum_id()) {
        return;
    }
    match ops::add_images(app, msg.channel_id.get(), urls).await {
        Ok(Some(_)) => {
            let _ = app.http.create_reaction(msg.channel_id, msg.id, &OK).await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, discord_post_id = msg.channel_id.get(), "画像の登録に失敗");
            let _ = app.http.create_reaction(msg.channel_id, msg.id, &NG).await;
            let text = format!("画像の登録に失敗しました: {e:#}\n`/sync ticket` で再同期できます");
            let _ = app
                .http
                .create_message(msg.channel_id)
                .reply(msg.id)
                .content(&render::truncate(&text, interact::CONTENT_MAX))
                .await;
        }
    }
}
