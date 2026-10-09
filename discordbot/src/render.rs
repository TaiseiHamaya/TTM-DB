//! チケットの Discord 表示 (Forum Post タイトル・タグ・先頭 Embed・ボタン) と Forum タグ管理。

use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;
use twilight_http::request::Request;
use twilight_http::routing::Route;
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
use twilight_model::channel::message::{AllowedMentions, Component, Embed, Message};
use twilight_model::channel::{Channel, forum::ForumTag};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, TagMarker, UserMarker};
use twilight_util::builder::embed::{
    EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder, ImageSource,
};

use crate::app::App;
use crate::config::{Col, Config};
use crate::masters::{Masters, StatusRole, can_transition};
use crate::store::{Table, Ticket};

pub const BUTTON_PREFIX: &str = "ttm:st:";
const FOOTER_UPDATED: &str = "更新 ";

/// Post タイトル "タイトル [T-0001]"。進行度は含めない (スレッド名の変更は Discord のレート制限が
/// 厳しいため、進行度のたびに変えるとすぐ制限に当たる。進行度はタグと Embed で表示する)。
/// スレッド名は100文字までなので、末尾の ID が切れないようタイトル側を切り詰める
pub fn post_title(t: &Ticket) -> String {
    let suffix = format!(" [{}]", t.id());
    let room = 100usize.saturating_sub(suffix.chars().count()).max(1);
    format!("{}{suffix}", truncate(t.get(Col::Title), room))
}

/// Post タイトル "... [T-0001]" からチケット ID を取り出す
pub fn ticket_id_from_title(title: &str) -> Option<&str> {
    let rest = title.trim_end().strip_suffix(']')?;
    let (_, id) = rest.rsplit_once('[')?;
    (!id.is_empty()).then_some(id)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// 指定ユーザーだけにメンション通知を送る設定
pub fn mention_only(users: impl IntoIterator<Item = Id<UserMarker>>) -> AllowedMentions {
    AllowedMentions {
        users: users.into_iter().collect(),
        ..Default::default()
    }
}

/// スプシ上の名前から Discord ユーザーを引く (設定の表示名対応 → masters の assignee の順)
pub fn user_by_name(name: &str, m: &Masters, cfg: &Config) -> Option<Id<UserMarker>> {
    if name.is_empty() {
        return None;
    }
    cfg.member_id_for_name(name)
        .or_else(|| m.assignee_by_name(name)?.discord_id)
        .filter(|&id| id != 0)
        .map(Id::new)
}

pub fn assignee_user(t: &Ticket, m: &Masters, cfg: &Config) -> Option<Id<UserMarker>> {
    user_by_name(t.get(Col::Assignee), m, cfg)
}

pub fn reporter_user(t: &Ticket, m: &Masters, cfg: &Config) -> Option<Id<UserMarker>> {
    user_by_name(t.get(Col::Reporter), m, cfg)
}

/// メンション (ユーザーが引けなければ名前のまま)
fn user_label(col: Col, t: &Ticket, m: &Masters, cfg: &Config) -> String {
    match user_by_name(t.get(col), m, cfg) {
        Some(id) => format!("<@{id}>"),
        None => t.get(col).to_owned(),
    }
}

fn assignee_label(t: &Ticket, m: &Masters, cfg: &Config) -> String {
    user_label(Col::Assignee, t, m, cfg)
}

fn link(t: &Ticket) -> String {
    let url = t.get(Col::DiscordUrl);
    if url.is_empty() {
        format!("`{}`", t.id())
    } else {
        format!("[{}]({url})", t.id())
    }
}

/// Embed のフッタから表示時点の updated_at を取り出す (LWW 判定用)
pub fn updated_at_from_embed(msg: &Message) -> Option<String> {
    let footer = msg.embeds.first()?.footer.as_ref()?;
    footer
        .text
        .split(" · ")
        .find_map(|p| p.strip_prefix(FOOTER_UPDATED))
        .map(str::to_owned)
}

pub fn embed(t: &Ticket, table: &Table, m: &Masters, cfg: &Config) -> Embed {
    let role = m.role_of(t.get(Col::Status));
    let field = |name: &str, value: String| EmbedFieldBuilder::new(name, value).inline().build();
    let mut e = EmbedBuilder::new()
        .title(truncate(&format!("{} {}", t.id(), t.get(Col::Title)), 256))
        .description(truncate(t.get(Col::Body), 4000))
        .color(role.map(StatusRole::color).unwrap_or(0x99aab5))
        .field(field("進行度", or_dash(t.get(Col::Status))))
        .field(field("種類", or_dash(t.get(Col::Category))))
        .field(field("担当者", or_dash(&assignee_label(t, m, cfg))))
        .field(field("優先度", or_dash(t.get(Col::Priority))))
        .field(field("期限", or_dash(t.get(Col::DueDate))))
        .field(field("発行者", or_dash(&user_label(Col::Reporter, t, m, cfg))));
    let parent = t.get(Col::ParentId);
    if !parent.is_empty() {
        let v = table
            .find(parent)
            .map(link)
            .unwrap_or_else(|| format!("`{parent}`"));
        e = e.field(field("親チケット", v));
    }
    let children: Vec<String> = table.children_of(t.id()).map(link).collect();
    if !children.is_empty() {
        e = e.field(EmbedFieldBuilder::new("子チケット", truncate(&children.join(" "), 1000)));
    }
    let images = t.image_urls();
    if !images.is_empty() {
        e = e.field(field("画像", format!("{}件", images.len())));
        if let Ok(src) = ImageSource::url(images[0]) {
            e = e.image(src);
        }
    }
    e.footer(EmbedFooterBuilder::new(format!(
        "{} · {FOOTER_UPDATED}{}",
        t.id(),
        t.get(Col::UpdatedAt)
    )))
    .build()
}

fn or_dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
}

pub fn button(custom_id: String, label: String, style: ButtonStyle, disabled: bool) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(custom_id),
        disabled,
        emoji: None,
        label: Some(label),
        style,
        url: None,
        sku_id: None,
    })
}

pub fn row(components: Vec<Component>) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components,
    })
}

/// 状態変更ボタン + 親チケットへの移動ボタン
pub fn components(t: &Ticket, table: &Table, m: &Masters) -> Vec<Component> {
    let current = m.role_of(t.get(Col::Status));
    let status_button = |role: StatusRole, label: String, style: ButtonStyle| {
        let enabled = current.is_some_and(|c| can_transition(c, role));
        button(
            format!("{BUTTON_PREFIX}{}:{}", t.id(), role.key()),
            label,
            style,
            !enabled,
        )
    };
    let name = |role| m.status_by_role(role).name.clone();
    let mut rows = vec![row(vec![
        status_button(
            StatusRole::InProgress,
            format!("{}にする", name(StatusRole::InProgress)),
            ButtonStyle::Primary,
        ),
        status_button(
            StatusRole::Done,
            format!("{}にする", name(StatusRole::Done)),
            ButtonStyle::Success,
        ),
        status_button(
            StatusRole::Suspended,
            name(StatusRole::Suspended),
            ButtonStyle::Secondary,
        ),
        status_button(
            StatusRole::Discarded,
            name(StatusRole::Discarded),
            ButtonStyle::Danger,
        ),
    ])];
    if let Some(parent) = table.find(t.get(Col::ParentId)) {
        let url = parent.get(Col::DiscordUrl);
        if !url.is_empty() {
            rows.push(row(vec![Component::Button(Button {
                id: None,
                custom_id: None,
                disabled: false,
                emoji: None,
                label: Some(format!("親チケット {} へ", parent.id())),
                style: ButtonStyle::Link,
                url: Some(url.to_owned()),
                sku_id: None,
            })]));
        }
    }
    rows
}

/// スプシで変更を通知する列 (スプシ側で編集される列のみ。タイトル等は保護されている)
const NOTIFY_COLS: [Col; 8] = [
    Col::Status,
    Col::Category,
    Col::Priority,
    Col::Assignee,
    Col::StartedAt,
    Col::DueDate,
    Col::CompletedAt,
    Col::Body,
];

/// スプシで変わった列 (前回 Discord へ反映した値 → 現在の値)
pub fn changed_cols(prev: &HashMap<String, String>, t: &Ticket) -> Vec<Col> {
    NOTIFY_COLS
        .into_iter()
        .filter(|&c| prev.get(c.key()).map_or("", String::as_str) != t.get(c))
        .collect()
}

/// スプシでの変更内容を Post に投稿する本文。変更が無ければ None
pub fn changes_content(
    prev: &HashMap<String, String>,
    t: &Ticket,
    m: &Masters,
    cfg: &Config,
) -> Option<String> {
    let cols = changed_cols(prev, t);
    if cols.is_empty() {
        return None;
    }
    let show = |col: Col, v: &str| -> String {
        match col {
            Col::Body => or_dash(&truncate(&v.split_whitespace().collect::<Vec<_>>().join(" "), 200)),
            _ => or_dash(&truncate(v, 100)),
        }
    };
    let mut lines = vec!["📝 スプシで更新されました".to_owned()];
    for col in &cols {
        let old = prev.get(col.key()).map_or("", String::as_str);
        lines.push(format!(
            "- **{}**: {} → {}",
            cfg.header(*col),
            show(*col, old),
            show(*col, t.get(*col))
        ));
    }
    // 担当者か進行度が変わったときは担当者に知らせる
    if cols.iter().any(|c| matches!(c, Col::Assignee | Col::Status)) {
        lines.push(format!("担当: {}", assignee_label(t, m, cfg)));
    }
    Some(truncate(&lines.join("\n"), 2000))
}

/// 先頭メッセージ本文 (担当者メンション)
pub fn starter_content(t: &Ticket, m: &Masters, cfg: &Config) -> String {
    format!("担当: {}", assignee_label(t, m, cfg))
}

/// Forum タグ名 (Discord の上限 20 文字に切り詰める)
fn tag_name(name: &str) -> String {
    truncate(name, 20)
}

pub async fn applied_tags(app: &App, t: &Ticket) -> Vec<Id<TagMarker>> {
    let tags = app.tags.read().await;
    [Col::Status, Col::Category, Col::Priority, Col::Assignee]
        .into_iter()
        .filter_map(|c| tags.get(&tag_name(t.get(c))).copied())
        .collect()
}

pub fn post_url(app: &App, post_id: Id<ChannelMarker>) -> String {
    format!(
        "https://discord.com/channels/{}/{}",
        app.env.guild_id, post_id
    )
}

async fn forum_tags(app: &App) -> Result<Vec<ForumTag>> {
    let ch = app.http.channel(app.forum_id()).await?.model().await?;
    Ok(ch.available_tags.unwrap_or_default())
}

/// Forum タグを丸ごと置き換える。既存タグは ID を付けて送り、付与済みタグが外れないようにする。
/// 新規タグは ID を持たないため、型付きの API ではなく JSON を直接送る
async fn put_forum_tags(app: &App, tags: Vec<serde_json::Value>) -> Result<()> {
    let req = Request::builder(&Route::UpdateChannel {
        channel_id: app.forum_id().get(),
    })
    .json(&json!({ "available_tags": tags }))
    .build()?;
    app.http.request::<Channel>(req).await?;
    Ok(())
}

/// masters を元に不足している Forum タグを追加し、タグ名 -> ID の対応を更新する。
/// `/sync tags` (cleanup = true) では、旧名→新名の rename 指定でタグ名を変更し (ID は維持)、
/// masters に無くなったタグを削除して上限の枠を空ける
pub async fn sync_forum_tags(app: &App, cleanup: bool) -> Result<Vec<String>> {
    let max = app.cfg.ticket.max_forum_tags;
    let m = app.masters.read().await.clone();
    let mut existing = forum_tags(app).await?;
    let mut report = Vec::new();
    let mut changed = false;

    if cleanup {
        for (old, new) in &m.renames {
            if existing.iter().any(|t| &t.name == new) {
                continue;
            }
            if let Some(tag) = existing.iter_mut().find(|t| &t.name == old) {
                tag.name = new.clone();
                changed = true;
                report.push(format!("タグ名変更: {old} → {new}"));
            }
        }
        // 統合された種類 (rename の新名が既にある旧名) や廃止された進行度のタグ
        let wanted: Vec<String> = m.tag_names(max).iter().map(|n| tag_name(n)).collect();
        existing.retain(|t| {
            let keep = wanted.contains(&t.name);
            if !keep {
                changed = true;
                report.push(format!("タグ削除: {}", t.name));
            }
            keep
        });
    }

    let mut values: Vec<serde_json::Value> = existing
        .iter()
        .map(|t| serde_json::to_value(t).unwrap_or_else(|_| json!({ "id": t.id, "name": t.name })))
        .collect();
    for name in m.tag_names(max).iter().map(|n| tag_name(n)) {
        if values.len() >= max {
            break;
        }
        if !existing.iter().any(|t| t.name == name) {
            values.push(json!({ "name": name }));
            changed = true;
            report.push(format!("タグ追加: {name}"));
        }
    }
    if changed {
        put_forum_tags(app, values).await?;
        existing = forum_tags(app).await?;
    }
    let map: HashMap<String, Id<TagMarker>> =
        existing.into_iter().map(|t| (t.name, t.id)).collect();
    *app.tags.write().await = map;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_from_title() {
        assert_eq!(ticket_id_from_title("ログイン [T-0001]"), Some("T-0001"));
        assert_eq!(ticket_id_from_title("[仮] ログイン [T-0001]"), Some("T-0001"));
        assert_eq!(ticket_id_from_title("雑談"), None);
        assert_eq!(ticket_id_from_title("x []"), None);
    }

    #[test]
    fn long_title_keeps_id() {
        let cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let long = "あ".repeat(200);
        let rows = vec![
            cfg.required_cols().iter().map(|c| cfg.header(*c).to_owned()).collect::<Vec<_>>(),
            cfg.required_cols()
                .iter()
                .map(|c| match c {
                    Col::TicketId => "T-0001".to_owned(),
                    Col::Title => long.clone(),
                    _ => String::new(),
                })
                .collect(),
        ];
        let table = Table::from_rows(&rows, &cfg).unwrap();
        let title = post_title(&table.tickets[0]);
        assert_eq!(title.chars().count(), 100);
        assert!(title.ends_with("… [T-0001]"));
        assert_eq!(ticket_id_from_title(&title), Some("T-0001"));
    }

    #[test]
    fn changes_from_sheets() {
        let cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let cols = cfg.required_cols();
        let row = |title: &str, category: &str, updated: &str| -> Vec<String> {
            cols.iter()
                .map(|c| match c {
                    Col::TicketId => "T-0001".to_owned(),
                    Col::Title => title.to_owned(),
                    Col::Category => category.to_owned(),
                    Col::UpdatedAt => updated.to_owned(),
                    _ => String::new(),
                })
                .collect()
        };
        let rows = vec![
            cols.iter().map(|c| cfg.header(*c).to_owned()).collect::<Vec<_>>(),
            row("タイトル", "旧", "a"),
            row("別タイトル", "新", "b"),
        ];
        let table = Table::from_rows(&rows, &cfg).unwrap();
        let prev = table.tickets[0].values();
        // タイトルと updated_at の変化は通知しない
        assert_eq!(changed_cols(&prev, &table.tickets[1]), vec![Col::Category]);
        assert!(changed_cols(&prev, &table.tickets[0]).is_empty());
        let m = Masters::default();
        let text = changes_content(&prev, &table.tickets[1], &m, &cfg).unwrap();
        assert!(text.contains("旧 → 新"));
    }
}
