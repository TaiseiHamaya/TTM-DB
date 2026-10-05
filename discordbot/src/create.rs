//! /ticket create の発行フロー。
//!
//! Discord のフォームは部品5つまでで、フォームの送信に続けて別のフォームを開くこともできない。
//! そのため次の2段構成にしている。
//! 1. フォーム: タイトル・詳細・期限・親チケット・画像 (テキストとファイルはフォームでしか入力できない)
//! 2. フォーム送信で自動的に出る本人だけのメッセージ: 種類・担当者・優先度を選んで「発行」
//!
//! 入力途中の内容は App::drafts に一時保存する (一定時間で破棄)。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use twilight_model::application::interaction::Interaction;
use twilight_model::application::interaction::message_component::MessageComponentInteractionData;
use twilight_model::application::interaction::modal::{
    ModalInteractionComponent, ModalInteractionData,
};
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::{
    ButtonStyle, FileUpload, Label, SelectDefaultValue, SelectMenu, SelectMenuOption,
    SelectMenuType, TextInput, TextInputStyle,
};
use twilight_model::http::interaction::InteractionResponseType;
use twilight_model::id::Id;
use twilight_util::builder::InteractionResponseDataBuilder;

use crate::app::{App, display_name};
use crate::interact::{self, respond};
use crate::masters::Masters;
use crate::ops::{self, NewImage, NewTicket};
use crate::render::{self, button, row, truncate};

/// フォームの custom_id。新規は "tm:new"、修正は "tm:new:<key>"。
/// 2段目のメッセージの部品は "tm:new:<key>:<操作>"
pub const PREFIX: &str = "tm:new";
const DRAFT_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_IMAGES: u8 = 10;

#[derive(Debug, Clone)]
pub struct Draft {
    user_id: u64,
    /// 発行者のスプシ上の表示名
    reporter: String,
    title: String,
    body: String,
    due: String,
    parent: String,
    images: Vec<DraftImage>,
    category: Option<String>,
    /// (Discord ユーザー ID, スプシ上の表示名)
    assignee: Option<(u64, String)>,
    priority: Option<String>,
    /// 入力の不備などの注意書き
    notice: Option<String>,
    /// 発行処理中 (「発行」の二度押し対策)
    issuing: bool,
    created: Instant,
}

#[derive(Debug, Clone)]
struct DraftImage {
    filename: String,
    url: String,
}

/// /ticket create: フォームを開く。チケットの Post 内なら親チケットを自動入力する
pub async fn open_form(app: &App, i: &Interaction) -> Result<()> {
    let parent = i
        .channel
        .as_ref()
        .filter(|ch| ch.parent_id == Some(app.forum_id()))
        .and_then(|ch| render::ticket_id_from_title(ch.name.as_deref()?))
        .unwrap_or_default()
        .to_owned();
    let due = (chrono::Utc::now().with_timezone(&app.cfg.tz()).date_naive() + chrono::Days::new(7))
        .format("%Y-%m-%d")
        .to_string();
    let form = Form {
        title: "",
        body: "",
        due: &due,
        parent: &parent,
    };
    respond(app, i, &form.response(PREFIX.to_owned())).await
}

/// フォームの初期値
struct Form<'a> {
    title: &'a str,
    body: &'a str,
    due: &'a str,
    parent: &'a str,
}

impl Form<'_> {
    fn response(&self, custom_id: String) -> twilight_model::http::interaction::InteractionResponse {
        let components = vec![
            label(
                "タイトル",
                None,
                text_input("title", TextInputStyle::Short, self.title, "ログインできない", true, 80),
            ),
            label(
                "詳細",
                None,
                text_input(
                    "body",
                    TextInputStyle::Paragraph,
                    self.body,
                    "再現手順・期待する結果など",
                    true,
                    4000,
                ),
            ),
            label(
                "期限",
                Some("YYYY-MM-DD (2026/10/10 や 20261010 も可)"),
                text_input("due", TextInputStyle::Short, self.due, "2026-10-10", true, 20),
            ),
            label(
                "親チケット",
                Some("子チケットにする場合のみ。チケットの Post 内で開くと自動入力されます"),
                text_input("parent", TextInputStyle::Short, self.parent, "T-0001", false, 30),
            ),
            label(
                "画像",
                Some("任意。最大10枚"),
                Component::FileUpload(FileUpload {
                    id: None,
                    custom_id: "images".into(),
                    max_values: Some(MAX_IMAGES),
                    min_values: Some(0),
                    required: Some(false),
                }),
            ),
        ];
        interact::response(
            InteractionResponseType::Modal,
            InteractionResponseDataBuilder::new()
                .custom_id(custom_id)
                .title("チケット発行 (1/2)")
                .components(components)
                .build(),
        )
    }
}

fn label(text: &str, description: Option<&str>, component: Component) -> Component {
    Component::Label(Label {
        id: None,
        label: text.into(),
        description: description.map(Into::into),
        component: Box::new(component),
    })
}

#[allow(deprecated)] // TextInput::label は Label 部品に置き換わったため使わない
fn text_input(
    custom_id: &str,
    style: TextInputStyle,
    value: &str,
    placeholder: &str,
    required: bool,
    max_length: u16,
) -> Component {
    Component::TextInput(TextInput {
        id: None,
        custom_id: custom_id.into(),
        label: None,
        max_length: Some(max_length),
        min_length: required.then_some(1),
        placeholder: Some(placeholder.into()),
        required: Some(required),
        style,
        value: (!value.is_empty()).then(|| value.to_owned()),
    })
}

/// フォームの入力値 (custom_id -> 値)。Label や ActionRow の入れ子を平らにする
#[derive(Default)]
struct Submitted {
    texts: HashMap<String, String>,
    files: HashMap<String, Vec<Id<twilight_model::id::marker::AttachmentMarker>>>,
}

impl Submitted {
    fn collect(&mut self, c: &ModalInteractionComponent) {
        match c {
            ModalInteractionComponent::Label(l) => self.collect(&l.component),
            ModalInteractionComponent::ActionRow(r) => {
                for c in &r.components {
                    self.collect(c);
                }
            }
            ModalInteractionComponent::TextInput(t) => {
                self.texts.insert(t.custom_id.clone(), t.value.trim().to_owned());
            }
            ModalInteractionComponent::FileUpload(f) => {
                self.files.insert(f.custom_id.clone(), f.values.clone());
            }
            _ => {}
        }
    }

    fn text(&self, id: &str) -> String {
        self.texts.get(id).cloned().unwrap_or_default()
    }
}

/// フォーム送信: 入力を保存し、種類・担当者・優先度の選択メッセージを出す (修正時は書き換える)
pub async fn on_modal(app: &App, i: &Interaction, data: &ModalInteractionData) -> Result<()> {
    let author = i.author().context("送信者が不明です")?;
    let user_id = author.id.get();
    let nick = i.member.as_ref().and_then(|m| m.nick.as_deref());
    let reporter = app
        .sheet_name_of(user_id, &author.name, display_name(nick, author))
        .await;
    let mut sub = Submitted::default();
    for c in &data.components {
        sub.collect(c);
    }
    let images: Vec<DraftImage> = sub
        .files
        .get("images")
        .into_iter()
        .flatten()
        .filter_map(|id| data.resolved.as_ref()?.attachments.get(id))
        .map(|a| DraftImage {
            filename: a.filename.clone(),
            url: a.url.clone(),
        })
        .collect();

    let editing = data.custom_id.strip_prefix(PREFIX).and_then(|s| s.strip_prefix(':'));
    let key = editing.map(str::to_owned).unwrap_or_else(|| i.id.to_string());
    let mut drafts = app.drafts.lock().await;
    drafts.retain(|_, d| d.created.elapsed() < DRAFT_TTL);
    if editing.is_some() && !drafts.contains_key(&key) {
        drop(drafts);
        return respond(app, i, &expired()).await;
    }
    let draft = drafts.entry(key.clone()).or_insert_with(|| Draft {
        user_id,
        reporter,
        title: String::new(),
        body: String::new(),
        due: String::new(),
        parent: String::new(),
        images: Vec::new(),
        category: None,
        assignee: None,
        priority: None,
        notice: None,
        issuing: false,
        created: Instant::now(),
    });
    draft.title = sub.text("title");
    draft.body = sub.text("body");
    draft.due = sub.text("due");
    draft.parent = sub.text("parent");
    // 修正フォームではファイルを再表示できないので、添付し直さなければ前回の画像を残す
    if editing.is_none() || !images.is_empty() {
        draft.images = images;
    }
    draft.notice = due_problem(&draft.due);
    let m = app.masters.read().await.clone();
    let (content, components) = step2(&key, draft, &m);
    drop(drafts);

    let res = if editing.is_some() {
        interact::update_message(&content, components)
    } else {
        interact::ephemeral_with(&content, components)
    };
    respond(app, i, &res).await
}

fn due_problem(due: &str) -> Option<String> {
    ops::normalize_date(due)
        .is_none()
        .then(|| format!("期限「{due}」は日付として読めません。「フォームを修正」から直してください"))
}

fn expired() -> twilight_model::http::interaction::InteractionResponse {
    interact::update_message(
        "入力の有効期限が切れました。もう一度 `/ticket create` を実行してください。",
        Vec::new(),
    )
}

/// 2段目のメッセージ (入力内容の確認 + 種類・担当者・優先度の選択 + 発行ボタン)
fn step2(key: &str, d: &Draft, m: &Masters) -> (String, Vec<Component>) {
    let id = |action: &str| format!("{PREFIX}:{key}:{action}");
    let mut content = format!(
        "**チケット発行 (2/2)** 種類・担当者・優先度を選んで「発行」を押してください。\n\
         > タイトル: {}\n> 期限: {}\n> 親チケット: {}\n> 画像: {}枚",
        truncate(&d.title, 80),
        or_none(&d.due),
        or_none(&d.parent),
        d.images.len(),
    );
    if let Some(n) = &d.notice {
        content.push_str(&format!("\n⚠ {n}"));
    }
    let components = vec![
        row(vec![text_select(&id("cat"), "種類", &m.categories, d.category.as_deref())]),
        row(vec![user_select(&id("asg"), "担当者", d.assignee.as_ref().map(|a| a.0))]),
        row(vec![text_select(&id("pri"), "優先度", &m.priorities, d.priority.as_deref())]),
        row(vec![
            button(id("go"), "発行".into(), ButtonStyle::Success, false),
            button(id("edit"), "フォームを修正".into(), ButtonStyle::Secondary, false),
            button(id("cancel"), "キャンセル".into(), ButtonStyle::Danger, false),
        ]),
    ];
    (content, components)
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "なし" } else { s }
}

fn text_select(custom_id: &str, placeholder: &str, items: &[String], selected: Option<&str>) -> Component {
    Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id: custom_id.into(),
        default_values: None,
        disabled: false,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(
            items
                .iter()
                .take(25)
                .map(|v| SelectMenuOption {
                    default: selected == Some(v.as_str()),
                    description: None,
                    emoji: None,
                    label: truncate(v, 100),
                    value: truncate(v, 100),
                })
                .collect(),
        ),
        placeholder: Some(placeholder.into()),
        required: None,
    })
}

fn user_select(custom_id: &str, placeholder: &str, selected: Option<u64>) -> Component {
    Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id: custom_id.into(),
        default_values: selected.map(|id| vec![SelectDefaultValue::User(Id::new(id))]),
        disabled: false,
        kind: SelectMenuType::User,
        max_values: Some(1),
        min_values: Some(1),
        options: None,
        placeholder: Some(placeholder.into()),
        required: None,
    })
}

/// 2段目のメッセージの選択・ボタン操作。`rest` は "<key>:<操作>"
pub async fn on_component(
    app: &App,
    i: &Interaction,
    data: &MessageComponentInteractionData,
    rest: &str,
) -> Result<()> {
    let Some((key, action)) = rest.rsplit_once(':') else {
        return Ok(());
    };
    let user_id = i.author_id().context("操作者が不明です")?.get();
    let mut drafts = app.drafts.lock().await;
    let Some(draft) = drafts.get_mut(key).filter(|d| d.user_id == user_id) else {
        drop(drafts);
        return respond(app, i, &expired()).await;
    };
    let m = app.masters.read().await.clone();
    match action {
        "cat" => draft.category = data.values.first().cloned(),
        "pri" => draft.priority = data.values.first().cloned(),
        "asg" => {
            let picked = data.values.first().and_then(|v| v.parse::<u64>().ok());
            let resolved = data.resolved.as_ref();
            let user = picked.and_then(|id| resolved?.users.get(&Id::new(id)));
            match user {
                Some(u) if u.bot => draft.notice = Some("Bot は担当者にできません".into()),
                Some(u) => {
                    let nick = resolved
                        .and_then(|r| r.members.get(&u.id))
                        .and_then(|m| m.nick.as_deref());
                    let name = app
                        .sheet_name_of(u.id.get(), &u.name, display_name(nick, u))
                        .await;
                    draft.assignee = Some((u.id.get(), name));
                    draft.notice = due_problem(&draft.due);
                }
                None => draft.assignee = None,
            }
        }
        "edit" => {
            let form = Form {
                title: &draft.title,
                body: &draft.body,
                due: &draft.due,
                parent: &draft.parent,
            };
            let res = form.response(format!("{PREFIX}:{key}"));
            drop(drafts);
            return respond(app, i, &res).await;
        }
        "cancel" => {
            drafts.remove(key);
            drop(drafts);
            let res = interact::update_message("発行をキャンセルしました。", Vec::new());
            return respond(app, i, &res).await;
        }
        "go" => {
            if draft.issuing {
                drop(drafts);
                let res = interact::response(
                    InteractionResponseType::DeferredUpdateMessage,
                    InteractionResponseDataBuilder::new().build(),
                );
                return respond(app, i, &res).await;
            }
            draft.issuing = true;
            let draft = draft.clone();
            drop(drafts);
            return issue(app, i, key, draft, &m).await;
        }
        _ => return Ok(()),
    }
    let (content, components) = step2(key, draft, &m);
    drop(drafts);
    respond(app, i, &interact::update_message(&content, components)).await
}

/// 「発行」: 未選択があれば知らせ、揃っていれば発行する
async fn issue(app: &App, i: &Interaction, key: &str, mut d: Draft, m: &Masters) -> Result<()> {
    let mut missing = Vec::new();
    if d.category.is_none() {
        missing.push("種類");
    }
    if d.assignee.is_none() {
        missing.push("担当者");
    }
    if d.priority.is_none() {
        missing.push("優先度");
    }
    if !missing.is_empty() || due_problem(&d.due).is_some() {
        d.notice = if missing.is_empty() {
            due_problem(&d.due)
        } else {
            Some(format!("{} を選んでください", missing.join("・")))
        };
        let (content, components) = step2(key, &d, m);
        save_notice(app, key, d.notice).await;
        return respond(app, i, &interact::update_message(&content, components)).await;
    }

    // Sheets と Discord への書き込みで3秒を超えるので先に応答しておく
    let res = interact::response(
        InteractionResponseType::DeferredUpdateMessage,
        InteractionResponseDataBuilder::new().build(),
    );
    respond(app, i, &res).await?;

    let result = async {
        let images = download_images(app, &d.images).await?;
        let (assignee_id, assignee_name) = d.assignee.clone().unwrap_or_default();
        let input = NewTicket {
            title: d.title.clone(),
            body: d.body.clone(),
            category: d.category.clone().unwrap_or_default(),
            assignee_name,
            assignee_id,
            reporter_name: d.reporter.clone(),
            reporter_id: d.user_id,
            priority: d.priority.clone().unwrap_or_default(),
            due_date: d.due.clone(),
            parent_id: Some(d.parent.clone()),
            images,
        };
        ops::create_ticket(app, input).await
    }
    .await;

    let (content, components) = match result {
        Ok(c) => {
            app.drafts.lock().await.remove(key);
            let mut s = format!("チケット **{}** を発行しました: {}", c.ticket_id, c.url);
            for w in c.warnings {
                s.push_str(&format!("\n⚠ {w}"));
            }
            (s, Vec::new())
        }
        Err(e) => {
            d.notice = Some(format!("発行に失敗しました: {e:#}"));
            save_notice(app, key, d.notice.clone()).await;
            step2(key, &d, m)
        }
    };
    let content = truncate(&content, interact::CONTENT_MAX);
    app.interaction()
        .update_response(&i.token)
        .content(Some(&content))
        .components(Some(&components))
        .await?;
    Ok(())
}

async fn save_notice(app: &App, key: &str, notice: Option<String>) {
    if let Some(d) = app.drafts.lock().await.get_mut(key) {
        d.notice = notice;
        d.issuing = false;
    }
}

/// フォームで添付された画像を取得する (Post に投稿し直すため)
async fn download_images(app: &App, images: &[DraftImage]) -> Result<Vec<NewImage>> {
    let mut out = Vec::new();
    for img in images {
        let bytes = app
            .web
            .get(&img.url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("画像 {} を取得できません", img.filename))?
            .bytes()
            .await?;
        out.push(NewImage {
            filename: img.filename.clone(),
            bytes: bytes.to_vec(),
            source_url: img.url.clone(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fits_discord_modal() {
        let form = Form {
            title: "",
            body: "",
            due: "2026-10-12",
            parent: "T-0001",
        };
        let json = serde_json::to_value(form.response(PREFIX.to_owned())).unwrap();
        assert_eq!(json["type"], 9);
        let components = json["data"]["components"].as_array().unwrap();
        // フォームの部品は5つまで
        assert_eq!(components.len(), 5);
        assert!(components.iter().all(|c| c["type"] == 18));
        assert_eq!(components[3]["component"]["value"], "T-0001");
        assert_eq!(components[4]["component"]["type"], 19);
        // 空の初期値は送らない
        assert!(components[0]["component"].get("value").is_none());
    }

    #[test]
    fn step2_rows() {
        let m = Masters {
            categories: vec!["バグ".into()],
            priorities: vec!["高".into()],
            ..Default::default()
        };
        let d = Draft {
            user_id: 1,
            reporter: "佐藤".into(),
            title: "t".into(),
            body: "b".into(),
            due: "あした".into(),
            parent: String::new(),
            images: Vec::new(),
            category: Some("バグ".into()),
            assignee: Some((42, "田中".into())),
            priority: None,
            notice: due_problem("あした"),
            issuing: false,
            created: Instant::now(),
        };
        let (content, rows) = step2("k", &d, &m);
        assert!(content.contains("⚠"));
        assert_eq!(rows.len(), 4);
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[0]["components"][0]["options"][0]["default"], true);
        assert_eq!(json[1]["components"][0]["default_values"][0]["id"], "42");
        assert_eq!(json[3]["components"][0]["custom_id"], "tm:new:k:go");
    }
}
