//! インタラクションへの応答の共通処理。

use std::time::Duration;

use anyhow::Result;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::{Component, MessageFlags};
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use twilight_util::builder::InteractionResponseDataBuilder;

use crate::app::App;
use crate::render::truncate;

/// Discord のメッセージ本文の上限
pub const CONTENT_MAX: usize = 2000;
/// 読み終わる頃に本人だけのメッセージを消すまでの時間
pub const DELETE_DELAY: Duration = Duration::from_secs(10);

pub fn response(
    kind: InteractionResponseType,
    data: InteractionResponseData,
) -> InteractionResponse {
    InteractionResponse {
        kind,
        data: Some(data),
    }
}

/// 本人にだけ見えるメッセージで応答する
pub fn ephemeral(content: &str) -> InteractionResponse {
    response(
        InteractionResponseType::ChannelMessageWithSource,
        InteractionResponseDataBuilder::new()
            .content(truncate(content, CONTENT_MAX))
            .flags(MessageFlags::EPHEMERAL)
            .build(),
    )
}

/// 本人にだけ見えるメッセージ (選択メニュー・ボタン付き)
pub fn ephemeral_with(content: &str, components: Vec<Component>) -> InteractionResponse {
    response(
        InteractionResponseType::ChannelMessageWithSource,
        InteractionResponseDataBuilder::new()
            .content(truncate(content, CONTENT_MAX))
            .components(components)
            .flags(MessageFlags::EPHEMERAL)
            .build(),
    )
}

/// 操作されたメッセージ自体を書き換えて応答する
pub fn update_message(content: &str, components: Vec<Component>) -> InteractionResponse {
    response(
        InteractionResponseType::UpdateMessage,
        InteractionResponseDataBuilder::new()
            .content(truncate(content, CONTENT_MAX))
            .components(components)
            .build(),
    )
}

/// 時間のかかる処理の前に「考え中」で応答する (本人にだけ見える)
pub fn defer_ephemeral() -> InteractionResponse {
    response(
        InteractionResponseType::DeferredChannelMessageWithSource,
        InteractionResponseDataBuilder::new()
            .flags(MessageFlags::EPHEMERAL)
            .build(),
    )
}

pub async fn respond(app: &App, i: &Interaction, res: &InteractionResponse) -> Result<()> {
    app.interaction()
        .create_response(i.id, &i.token, res)
        .await?;
    Ok(())
}

/// defer した応答の本文を書き換える
pub async fn edit_reply(app: &App, i: &Interaction, content: &str) -> Result<()> {
    let content = truncate(content, CONTENT_MAX);
    app.interaction()
        .update_response(&i.token)
        .content(Some(&content))
        .await?;
    Ok(())
}

/// 応答に続けて本人にだけ見えるメッセージを追加する
pub async fn followup(app: &App, i: &Interaction, content: &str) -> Result<()> {
    let content = truncate(content, CONTENT_MAX);
    app.interaction()
        .create_followup(&i.token)
        .content(&content)
        .flags(MessageFlags::EPHEMERAL)
        .await?;
    Ok(())
}

/// 応答のメッセージを消す。ボタン・フォーム送信への応答なら操作されたメッセージが消える。
/// 消せるのはインタラクションから15分以内
pub async fn delete_reply(app: &App, i: &Interaction) {
    if let Err(e) = app.interaction().delete_response(&i.token).await {
        tracing::warn!(error = %e, "応答メッセージの削除に失敗");
    }
}

/// 読む時間を置いてから応答のメッセージを消す
pub async fn delete_reply_later(app: &App, i: &Interaction) {
    tokio::time::sleep(DELETE_DELAY).await;
    delete_reply(app, i).await;
}

/// 処理中のエラーを本人に知らせる (応答済みかどうか分からないので両方試す)
pub async fn report_error(app: &App, i: &Interaction, err: &anyhow::Error) {
    let text = format!("エラーが発生しました: {err:#}");
    let shown =
        respond(app, i, &ephemeral(&text)).await.is_ok() || edit_reply(app, i, &text).await.is_ok();
    if shown {
        delete_reply_later(app, i).await;
    }
}
