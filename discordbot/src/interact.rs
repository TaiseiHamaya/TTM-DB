//! インタラクションへの応答の共通処理。

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

/// 処理中のエラーを本人に知らせる (応答済みかどうか分からないので両方試す)
pub async fn report_error(app: &App, i: &Interaction, err: &anyhow::Error) {
    let text = format!("エラーが発生しました: {err:#}");
    if respond(app, i, &ephemeral(&text)).await.is_err() {
        let _ = edit_reply(app, i, &text).await;
    }
}
