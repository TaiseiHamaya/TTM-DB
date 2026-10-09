//! /ticket create (とメッセージの右クリックメニュー「チケットを発行」) の発行フロー。
//!
//! Discord のフォームは部品5つまでで、フォームの送信に続けて別のフォームを開くこともできない。
//! そのため次の2段構成にしている。
//! 1. フォーム: タイトル・詳細・種類・担当者・優先度
//! 2. フォーム送信で自動的に出る本人だけのメッセージ: 内容を確認して「発行」。
//!    期限 (初期値は1週間後) と親チケット (Post 内なら自動設定) は「期限・親を変更」で開くフォームで直す
//!
//! 入力途中の内容は App::drafts に一時保存する (一定時間で破棄)。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use twilight_model::application::interaction::Interaction;
use twilight_model::application::interaction::modal::{
    ModalInteractionComponent, ModalInteractionData,
};
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::{
    ButtonStyle, Label, SelectDefaultValue, SelectMenu, SelectMenuOption, SelectMenuType,
    TextInput, TextInputStyle,
};
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::Id;
use twilight_model::id::marker::UserMarker;
use twilight_util::builder::InteractionResponseDataBuilder;

use crate::app::{App, display_name};
use crate::interact::{self, respond};
use crate::masters::Masters;
use crate::ops::{self, NewTicket};
use crate::render::{self, button, row, truncate};

/// フォームの custom_id。新規は "ttm:new"、修正は "ttm:new:<key>"、期限・親は "ttm:new:<key>:extra"。
/// 2段目のメッセージの部品は "ttm:new:<key>:<操作>"
pub const PREFIX: &str = "ttm:new";
const DRAFT_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct Draft {
    user_id: u64,
    /// 発行者のスプシ上の表示名
    reporter: String,
    title: String,
    body: String,
    due: String,
    parent: String,
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

/// メッセージの右クリックメニューに出すコマンド名。/ticket create と同じく空のフォームを開く
pub const MESSAGE_COMMAND: &str = "チケットを発行";

/// /ticket create: フォームを開く
pub async fn open_form(app: &App, i: &Interaction) -> Result<()> {
    let m = app.masters.read().await.clone();
    let form = Form {
        title: "",
        body: "",
        category: None,
        assignee: None,
        priority: None,
    };
    respond(app, i, &form.response(PREFIX.to_owned(), &m)).await
}

/// フォームの初期値
struct Form<'a> {
    title: &'a str,
    body: &'a str,
    category: Option<&'a str>,
    assignee: Option<u64>,
    priority: Option<&'a str>,
}

impl Form<'_> {
    fn response(&self, custom_id: String, m: &Masters) -> InteractionResponse {
        let components = vec![
            label(
                "タスク",
                None,
                text_input("title", TextInputStyle::Short, self.title, "", true, 80),
            ),
            label(
                "詳細",
                None,
                text_input("body", TextInputStyle::Paragraph, self.body, "", true, 4000),
            ),
            label(
                "種類",
                None,
                text_select("cat", "種類を選ぶ", &m.categories, self.category),
            ),
            label(
                "担当者",
                None,
                user_select("asg", "担当者を選ぶ", self.assignee),
            ),
            label(
                "優先度",
                None,
                text_select("pri", "優先度を選ぶ", &m.priorities, self.priority),
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

/// 2段目の「期限・親を変更」で開くフォーム
fn extra_form(key: &str, due: &str, parent: &str) -> InteractionResponse {
    interact::response(
        InteractionResponseType::Modal,
        InteractionResponseDataBuilder::new()
            .custom_id(format!("{PREFIX}:{key}:extra"))
            .title("期限・親チケットを変更")
            .components(vec![
                label(
                    "期限",
                    Some("YYYY-MM-DD (2026/10/10 や 20261010 も可)"),
                    text_input("due", TextInputStyle::Short, due, "2026-10-10", true, 20),
                ),
                label(
                    "親チケット",
                    Some("子チケットにする場合のみ。空にすると親なしで発行します"),
                    text_input("parent", TextInputStyle::Short, parent, "T-0001", false, 30),
                ),
            ])
            .build(),
    )
}

/// 期限の初期値 (1週間後)
fn default_due(app: &App) -> String {
    (chrono::Utc::now().with_timezone(&app.cfg.tz()).date_naive() + chrono::Days::new(7))
        .format("%Y-%m-%d")
        .to_string()
}

/// チケットの Post 内で実行されたら、その Post のチケット ID
fn parent_from_channel(app: &App, i: &Interaction) -> String {
    i.channel
        .as_ref()
        .filter(|ch| ch.parent_id == Some(app.forum_id()))
        .and_then(|ch| render::ticket_id_from_title(ch.name.as_deref()?))
        .unwrap_or_default()
        .to_owned()
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

fn text_select(
    custom_id: &str,
    placeholder: &str,
    items: &[String],
    selected: Option<&str>,
) -> Component {
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
        required: Some(true),
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
        required: Some(true),
    })
}

/// フォームの入力値 (custom_id -> 値)。Label や ActionRow の入れ子を平らにする
#[derive(Default)]
struct Submitted {
    texts: HashMap<String, String>,
    selects: HashMap<String, String>,
    users: HashMap<String, Id<UserMarker>>,
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
                self.texts
                    .insert(t.custom_id.clone(), t.value.trim().to_owned());
            }
            ModalInteractionComponent::StringSelect(s) => {
                if let Some(v) = s.values.first() {
                    self.selects.insert(s.custom_id.clone(), v.clone());
                }
            }
            ModalInteractionComponent::UserSelect(s) => {
                if let Some(v) = s.values.first() {
                    self.users.insert(s.custom_id.clone(), *v);
                }
            }
            _ => {}
        }
    }

    fn text(&self, id: &str) -> String {
        self.texts.get(id).cloned().unwrap_or_default()
    }
}

/// フォーム送信: 入力を保存し、確認メッセージを出す (修正時は書き換える)。
/// 新規なら期限は1週間後、親チケットはチケットの Post 内で実行されたときその Post のチケットにしておく
pub async fn on_modal(app: &App, i: &Interaction, data: &ModalInteractionData) -> Result<()> {
    let author = i.author().context("送信者が不明です")?;
    let user_id = author.id.get();
    let mut sub = Submitted::default();
    for c in &data.components {
        sub.collect(c);
    }

    let editing = data
        .custom_id
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_prefix(':'));
    if let Some(key) = editing.and_then(|s| s.strip_suffix(":extra")) {
        return on_extra_modal(app, i, key, user_id, &sub).await;
    }

    let nick = i.member.as_ref().and_then(|m| m.nick.as_deref());
    let reporter = app
        .sheet_name_of(user_id, &author.name, display_name(nick, author))
        .await;
    // 担当者 (スプシ上の表示名を引くのでロックの前に済ませる)
    let mut notice = None;
    let resolved = data.resolved.as_ref();
    let assignee = match sub.users.get("asg").and_then(|id| resolved?.users.get(id)) {
        Some(u) if u.bot => {
            notice =
                Some("Bot は担当者にできません。「フォームを修正」から選び直してください".into());
            None
        }
        Some(u) => {
            let nick = resolved
                .and_then(|r| r.members.get(&u.id))
                .and_then(|m| m.nick.as_deref());
            let name = app
                .sheet_name_of(u.id.get(), &u.name, display_name(nick, u))
                .await;
            Some((u.id.get(), name))
        }
        None => None,
    };

    let key = editing
        .map(str::to_owned)
        .unwrap_or_else(|| i.id.to_string());
    let mut drafts = app.drafts.lock().await;
    drafts.retain(|_, d| d.created.elapsed() < DRAFT_TTL);
    if editing.is_some() && !drafts.contains_key(&key) {
        drop(drafts);
        return respond_expired(app, i).await;
    }
    let draft = drafts.entry(key.clone()).or_insert_with(|| Draft {
        user_id,
        reporter,
        title: String::new(),
        body: String::new(),
        due: default_due(app),
        parent: parent_from_channel(app, i),
        category: None,
        assignee: None,
        priority: None,
        notice: None,
        issuing: false,
        created: Instant::now(),
    });
    draft.title = sub.text("title");
    draft.body = sub.text("body");
    draft.category = sub.selects.get("cat").cloned();
    draft.priority = sub.selects.get("pri").cloned();
    draft.assignee = assignee;
    draft.notice = notice.or_else(|| due_problem(&draft.due));
    let (content, components) = step2(&key, draft);
    drop(drafts);

    let res = if editing.is_some() {
        interact::update_message(&content, components)
    } else {
        interact::ephemeral_with(&content, components)
    };
    respond(app, i, &res).await
}

/// 期限・親のフォーム送信: 保存して確認メッセージを書き換える
async fn on_extra_modal(
    app: &App,
    i: &Interaction,
    key: &str,
    user_id: u64,
    sub: &Submitted,
) -> Result<()> {
    let mut drafts = app.drafts.lock().await;
    let Some(draft) = drafts.get_mut(key).filter(|d| d.user_id == user_id) else {
        drop(drafts);
        return respond_expired(app, i).await;
    };
    draft.due = sub.text("due");
    draft.parent = sub.text("parent");
    draft.notice = due_problem(&draft.due);
    let (content, components) = step2(key, draft);
    drop(drafts);
    respond(app, i, &interact::update_message(&content, components)).await
}

fn due_problem(due: &str) -> Option<String> {
    ops::normalize_date(due).is_none().then(|| {
        format!("期限「{due}」は日付として読めません。「期限・親を変更」から直してください")
    })
}

/// 確認メッセージを期限切れの案内に書き換え、少し置いて消す
async fn respond_expired(app: &App, i: &Interaction) -> Result<()> {
    let res = interact::update_message(
        "入力の有効期限が切れました。もう一度 `/ticket create` を実行してください。",
        Vec::new(),
    );
    respond(app, i, &res).await?;
    interact::delete_reply_later(app, i).await;
    Ok(())
}

/// 2段目のメッセージ (入力内容の確認 + 発行ボタン)
fn step2(key: &str, d: &Draft) -> (String, Vec<Component>) {
    let id = |action: &str| format!("{PREFIX}:{key}:{action}");
    let mut content = format!(
        "**チケット発行 (2/2)** 内容を確認して「発行」を押してください。\n\
         > タイトル: {}\n> 種類: {}\n> 担当者: {}\n> 優先度: {}\n> 期限: {}\n> 親チケット: {}",
        truncate(&d.title, 80),
        or_none(d.category.as_deref().unwrap_or_default()),
        or_none(
            d.assignee
                .as_ref()
                .map(|a| a.1.as_str())
                .unwrap_or_default()
        ),
        or_none(d.priority.as_deref().unwrap_or_default()),
        or_none(&d.due),
        or_none(&d.parent),
    );
    if let Some(n) = &d.notice {
        content.push_str(&format!("\n⚠ {n}"));
    }
    let components = vec![row(vec![
        button(id("go"), "発行".into(), ButtonStyle::Success, false),
        button(
            id("extra"),
            "期限・親を変更".into(),
            ButtonStyle::Primary,
            false,
        ),
        button(
            id("edit"),
            "フォームを修正".into(),
            ButtonStyle::Secondary,
            false,
        ),
        button(
            id("cancel"),
            "キャンセル".into(),
            ButtonStyle::Danger,
            false,
        ),
    ])];
    (content, components)
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "なし" } else { s }
}

/// 2段目のメッセージのボタン操作。`rest` は "<key>:<操作>"
pub async fn on_component(app: &App, i: &Interaction, rest: &str) -> Result<()> {
    let Some((key, action)) = rest.rsplit_once(':') else {
        return Ok(());
    };
    let user_id = i.author_id().context("操作者が不明です")?.get();
    let mut drafts = app.drafts.lock().await;
    let Some(draft) = drafts.get_mut(key).filter(|d| d.user_id == user_id) else {
        drop(drafts);
        return respond_expired(app, i).await;
    };
    match action {
        "edit" => {
            let m = app.masters.read().await.clone();
            let form = Form {
                title: &draft.title,
                body: &draft.body,
                category: draft.category.as_deref(),
                assignee: draft.assignee.as_ref().map(|a| a.0),
                priority: draft.priority.as_deref(),
            };
            let res = form.response(format!("{PREFIX}:{key}"), &m);
            drop(drafts);
            respond(app, i, &res).await
        }
        "extra" => {
            let res = extra_form(key, &draft.due, &draft.parent);
            drop(drafts);
            respond(app, i, &res).await
        }
        "cancel" => {
            drafts.remove(key);
            drop(drafts);
            respond(app, i, &deferred_update()).await?;
            interact::delete_reply(app, i).await;
            Ok(())
        }
        "go" => {
            if draft.issuing {
                drop(drafts);
                return respond(app, i, &deferred_update()).await;
            }
            draft.issuing = true;
            let draft = draft.clone();
            drop(drafts);
            issue(app, i, key, draft).await
        }
        _ => Ok(()),
    }
}

/// 「発行」: 未選択や期限の不備があれば知らせ、揃っていれば発行する
async fn issue(app: &App, i: &Interaction, key: &str, mut d: Draft) -> Result<()> {
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
            Some(format!(
                "「フォームを修正」から {} を選んでください",
                missing.join("・")
            ))
        };
        let (content, components) = step2(key, &d);
        save_notice(app, key, d.notice).await;
        return respond(app, i, &interact::update_message(&content, components)).await;
    }

    // Sheets と Discord への書き込みで3秒を超えるので先に応答しておく
    respond(app, i, &deferred_update()).await?;

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
    };
    let created = ops::create_ticket(app, input).await;
    let issued = created.is_ok();
    let (content, components) = match created {
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
            step2(key, &d)
        }
    };
    let content = truncate(&content, interact::CONTENT_MAX);
    app.interaction()
        .update_response(&i.token)
        .content(Some(&content))
        .components(Some(&components))
        .await?;
    // 失敗時は確認メッセージから直せるよう残す
    if issued {
        interact::delete_reply_later(app, i).await;
    }
    Ok(())
}

/// 操作されたメッセージを書き換えずに応答だけ返す
fn deferred_update() -> InteractionResponse {
    interact::response(
        InteractionResponseType::DeferredUpdateMessage,
        InteractionResponseDataBuilder::new().build(),
    )
}

async fn save_notice(app: &App, key: &str, notice: Option<String>) {
    if let Some(d) = app.drafts.lock().await.get_mut(key) {
        d.notice = notice;
        d.issuing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fits_discord_modal() {
        let m = Masters {
            categories: vec!["バグ".into(), "要望".into()],
            priorities: vec!["高".into()],
            ..Default::default()
        };
        let form = Form {
            title: "",
            body: "",
            category: Some("要望"),
            assignee: Some(42),
            priority: None,
        };
        let json = serde_json::to_value(form.response(PREFIX.to_owned(), &m)).unwrap();
        assert_eq!(json["type"], 9);
        let components = json["data"]["components"].as_array().unwrap();
        // フォームの部品は5つまで
        assert_eq!(components.len(), 5);
        assert!(components.iter().all(|c| c["type"] == 18));
        // 空の初期値は送らない
        assert!(components[0]["component"].get("value").is_none());
        assert_eq!(components[2]["component"]["custom_id"], "cat");
        assert_eq!(components[2]["component"]["options"][1]["default"], true);
        assert_eq!(components[3]["component"]["type"], 5);
        assert_eq!(components[3]["component"]["default_values"][0]["id"], "42");
        assert_eq!(components[4]["component"]["required"], true);
    }

    #[test]
    fn extra_form_keeps_values() {
        let json = serde_json::to_value(extra_form("k", "2026-10-16", "T-0001")).unwrap();
        assert_eq!(json["type"], 9);
        assert_eq!(json["data"]["custom_id"], "ttm:new:k:extra");
        let components = json["data"]["components"].as_array().unwrap();
        assert_eq!(components[0]["component"]["value"], "2026-10-16");
        assert_eq!(components[1]["component"]["value"], "T-0001");
    }

    #[test]
    fn step2_buttons() {
        let d = Draft {
            user_id: 1,
            reporter: "佐藤".into(),
            title: "t".into(),
            body: "b".into(),
            due: "あした".into(),
            parent: String::new(),
            category: Some("バグ".into()),
            assignee: Some((42, "田中".into())),
            priority: None,
            notice: due_problem("あした"),
            issuing: false,
            created: Instant::now(),
        };
        let (content, rows) = step2("k", &d);
        assert!(content.contains("⚠"));
        assert!(content.contains("担当者: 田中"));
        assert!(content.contains("優先度: なし"));
        assert_eq!(rows.len(), 1);
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[0]["components"][0]["custom_id"], "ttm:new:k:go");
        assert_eq!(json[0]["components"][1]["custom_id"], "ttm:new:k:extra");
    }
}
