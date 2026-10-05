//! Slash Commands: /ticket create, /ticket status, /list, /search, /sync

use anyhow::Result;
use twilight_model::application::command::{
    Command, CommandOptionChoice, CommandOptionChoiceValue, CommandType,
};
use twilight_model::application::interaction::Interaction;
use twilight_model::application::interaction::application_command::{
    CommandData, CommandDataOption, CommandOptionValue,
};
use twilight_model::http::interaction::InteractionResponseType;
use twilight_model::id::Id;
use twilight_model::id::marker::UserMarker;
use twilight_util::builder::InteractionResponseDataBuilder;
use twilight_util::builder::command::{
    CommandBuilder, StringBuilder, SubCommandBuilder, UserBuilder,
};
use twilight_util::builder::embed::{EmbedBuilder, EmbedFooterBuilder};

use crate::app::{App, display_name, is_admin};
use crate::config::Col;
use crate::interact::{self, defer_ephemeral, edit_reply, followup, respond};
use crate::store::Ticket;
use crate::{create, ops, render::truncate};

/// サーバーに登録するコマンド
pub fn definitions() -> Vec<Command> {
    let ticket_id = || StringBuilder::new("id", "チケットID").required(true).autocomplete(true);
    vec![
        CommandBuilder::new("ticket", "チケット", CommandType::ChatInput)
            .option(SubCommandBuilder::new(
                "create",
                "チケットを発行する (フォームが開きます)",
            ))
            .option(
                SubCommandBuilder::new("status", "進行度を変更する (ボタンが使えない場合の代替)")
                    .option(ticket_id())
                    .option(
                        StringBuilder::new("status", "変更後の進行度")
                            .required(true)
                            .autocomplete(true),
                    ),
            )
            .build(),
        CommandBuilder::new(
            "list",
            "チケット一覧 (進行度・種類・担当者・優先度で絞り込み)",
            CommandType::ChatInput,
        )
        .option(StringBuilder::new("status", "進行度").autocomplete(true))
        .option(StringBuilder::new("category", "種類").autocomplete(true))
        .option(UserBuilder::new("assignee", "担当者"))
        .option(StringBuilder::new("priority", "優先度").autocomplete(true))
        .build(),
        CommandBuilder::new(
            "search",
            "タイトル・詳細・チケットIDの部分一致で検索",
            CommandType::ChatInput,
        )
        .option(StringBuilder::new("query", "検索語").required(true))
        .build(),
        CommandBuilder::new("sync", "スプシとの同期", CommandType::ChatInput)
            .option(
                SubCommandBuilder::new("ticket", "指定チケットを Sheets から即時同期する")
                    .option(ticket_id()),
            )
            .option(SubCommandBuilder::new(
                "all",
                "Sheets を読み直して全チケットを強制同期する (管理者)",
            ))
            .option(SubCommandBuilder::new(
                "check",
                "マスタ不整合・必須欠落・ID重複を検査する",
            ))
            .option(SubCommandBuilder::new(
                "tags",
                "masters の内容 (名称変更含む) を Forum タグへ反映する (管理者)",
            ))
            .option(SubCommandBuilder::new(
                "members",
                "サーバー参加者を担当者マスタに取り込む (管理者)",
            ))
            .build(),
    ]
}

/// サブコマンド名とその引数
fn split(data: &CommandData) -> (Option<&str>, &[CommandDataOption]) {
    match data.options.first() {
        Some(CommandDataOption {
            name,
            value: CommandOptionValue::SubCommand(opts),
        }) => (Some(name.as_str()), opts),
        _ => (None, &data.options),
    }
}

fn str_opt(opts: &[CommandDataOption], name: &str) -> Option<String> {
    opts.iter().find(|o| o.name == name).and_then(|o| match &o.value {
        CommandOptionValue::String(s) | CommandOptionValue::Focused(s, _) => Some(s.clone()),
        _ => None,
    })
}

fn user_opt(opts: &[CommandDataOption], name: &str) -> Option<Id<UserMarker>> {
    opts.iter().find(|o| o.name == name).and_then(|o| match o.value {
        CommandOptionValue::User(id) => Some(id),
        _ => None,
    })
}

// ---------- オートコンプリート ----------

pub async fn on_autocomplete(app: &App, i: &Interaction, data: &CommandData) -> Result<()> {
    let (_, opts) = split(data);
    let Some((name, partial)) = opts.iter().find_map(|o| match &o.value {
        CommandOptionValue::Focused(v, _) => Some((o.name.as_str(), v.as_str())),
        _ => None,
    }) else {
        return Ok(());
    };
    let choices = match name {
        "category" => choices(app.masters.read().await.categories.clone(), partial),
        "priority" => choices(app.masters.read().await.priorities.clone(), partial),
        "status" => {
            let names = app
                .masters
                .read()
                .await
                .statuses
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>();
            choices(names, partial)
        }
        "id" => ticket_choices(app, i, partial).await,
        _ => Vec::new(),
    };
    let res = interact::response(
        InteractionResponseType::ApplicationCommandAutocompleteResult,
        InteractionResponseDataBuilder::new().choices(choices).build(),
    );
    respond(app, i, &res).await
}

fn choice(name: String, value: String) -> CommandOptionChoice {
    CommandOptionChoice {
        name: truncate(&name, 100),
        name_localizations: None,
        value: CommandOptionChoiceValue::String(value),
    }
}

fn choices(items: impl IntoIterator<Item = String>, partial: &str) -> Vec<CommandOptionChoice> {
    let partial = partial.to_lowercase();
    items
        .into_iter()
        .filter(|s| s.to_lowercase().contains(&partial))
        .take(25)
        .map(|s| choice(s.clone(), s))
        .collect()
}

/// チケットの候補。チケットの Post 内で使われたら、そのチケットを先頭に出す
async fn ticket_choices(app: &App, i: &Interaction, partial: &str) -> Vec<CommandOptionChoice> {
    let Ok(table) = app.cached_table().await else {
        return Vec::new();
    };
    let partial = partial.trim().to_lowercase();
    let here = i
        .channel
        .as_ref()
        .and_then(|ch| table.find_by_post(ch.id.get()));
    here.into_iter()
        .chain(
            table
                .tickets
                .iter()
                .rev()
                .filter(|t| here.is_none_or(|h| h.row != t.row)),
        )
        .filter(|t| !t.id().is_empty())
        .filter(|t| {
            t.id().to_lowercase().contains(&partial)
                || t.get(Col::Title).to_lowercase().contains(&partial)
        })
        .take(25)
        .map(|t| choice(format!("{} {}", t.id(), t.get(Col::Title)), t.id().to_owned()))
        .collect()
}

// ---------- コマンド本体 ----------

pub async fn on_command(app: &App, i: &Interaction, data: &CommandData) -> Result<()> {
    let (sub, opts) = split(data);
    match (data.name.as_str(), sub) {
        ("ticket", Some("create")) => create::open_form(app, i).await,
        ("ticket", Some("status")) => status(app, i, opts).await,
        ("list", _) => list(app, i, data, opts).await,
        ("search", _) => search(app, i, opts).await,
        ("sync", Some(sub)) => sync(app, i, sub, opts).await,
        _ => Ok(()),
    }
}

/// /ticket status: 進行度を変更する (ボタンが使えない場合の代替)
async fn status(app: &App, i: &Interaction, opts: &[CommandDataOption]) -> Result<()> {
    respond(app, i, &defer_ephemeral()).await?;
    let id = str_opt(opts, "id").unwrap_or_default();
    let status = str_opt(opts, "status").unwrap_or_default();
    let Some(role) = app.masters.read().await.role_of(&status) else {
        return edit_reply(app, i, &format!("進行度「{status}」は選択肢にありません")).await;
    };
    let actor = i.author_id().unwrap_or(Id::new(1));
    let admin = is_admin(i.member.as_ref().and_then(|m| m.permissions));
    let reply = match ops::change_status(app, &id, role, actor, admin, None).await {
        Ok(ops::StatusOutcome::Changed { from, to }) => {
            format!("{id}: 「{from}」→「{to}」に変更しました")
        }
        Err(e) => format!("変更できませんでした: {e:#}"),
    };
    edit_reply(app, i, &reply).await
}

fn line(t: &Ticket) -> String {
    let url = t.get(Col::DiscordUrl);
    let title = truncate(t.get(Col::Title), 40);
    let title = if url.is_empty() {
        title
    } else {
        format!("[{title}]({url})")
    };
    format!(
        "`{}` **[{}]** {} — {} / {} / 期限 {}",
        t.id(),
        t.get(Col::Status),
        title,
        t.get(Col::Assignee),
        t.get(Col::Priority),
        t.get(Col::DueDate),
    )
}

async fn send_list(app: &App, i: &Interaction, heading: String, mut items: Vec<Ticket>) -> Result<()> {
    let limit = app.cfg.ticket.list_limit;
    {
        let m = app.masters.read().await;
        items.sort_by(|a, b| {
            m.priority_rank(a.get(Col::Priority))
                .cmp(&m.priority_rank(b.get(Col::Priority)))
                .then_with(|| a.get(Col::DueDate).cmp(b.get(Col::DueDate)))
        });
    }
    let total = items.len();
    let body = if items.is_empty() {
        "該当するチケットはありません".to_owned()
    } else {
        items
            .iter()
            .take(limit)
            .map(line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let more = if total > limit {
        format!("\n…ほか {} 件", total - limit)
    } else {
        String::new()
    };
    let embed = EmbedBuilder::new()
        .title(truncate(&format!("{heading} ({total}件)"), 256))
        .description(truncate(&format!("{body}{more}"), 4000))
        .url(app.sheets.spreadsheet_url())
        .footer(EmbedFooterBuilder::new(
            "タイトルをクリックでスプレッドシートを開きます",
        ))
        .build();
    app.interaction()
        .update_response(&i.token)
        .embeds(Some(&[embed]))
        .await?;
    Ok(())
}

/// /list: チケット一覧
async fn list(app: &App, i: &Interaction, data: &CommandData, opts: &[CommandDataOption]) -> Result<()> {
    respond(app, i, &defer_ephemeral()).await?;
    let table = app.load_table().await?;
    let status = str_opt(opts, "status");
    let category = str_opt(opts, "category");
    let priority = str_opt(opts, "priority");
    let assignee = user_opt(opts, "assignee");
    let assignee_name = match assignee {
        Some(id) => {
            let resolved = data.resolved.as_ref();
            let user = resolved.and_then(|r| r.users.get(&id));
            let nick = resolved
                .and_then(|r| r.members.get(&id))
                .and_then(|m| m.nick.as_deref());
            let (username, display) = match user {
                Some(u) => (u.name.as_str(), display_name(nick, u)),
                None => ("", ""),
            };
            Some(app.sheet_name_of(id.get(), username, display).await)
        }
        None => None,
    };
    let eq = |filter: &Option<String>, v: &str| filter.as_ref().is_none_or(|f| f == v);
    let items: Vec<Ticket> = table
        .tickets
        .iter()
        .filter(|t| !t.id().is_empty())
        .filter(|t| eq(&status, t.get(Col::Status)))
        .filter(|t| eq(&category, t.get(Col::Category)))
        .filter(|t| eq(&priority, t.get(Col::Priority)))
        .filter(|t| {
            assignee.is_none() || assignee_name.as_deref() == Some(t.get(Col::Assignee))
        })
        .cloned()
        .collect();
    let mut conds: Vec<String> = [status, category, priority].into_iter().flatten().collect();
    conds.extend(assignee_name);
    let heading = if conds.is_empty() {
        "チケット一覧".to_owned()
    } else {
        format!("チケット一覧: {}", conds.join(" / "))
    };
    send_list(app, i, heading, items).await
}

/// /search: タイトル・詳細・チケットIDの部分一致で検索
async fn search(app: &App, i: &Interaction, opts: &[CommandDataOption]) -> Result<()> {
    respond(app, i, &defer_ephemeral()).await?;
    let query = str_opt(opts, "query").unwrap_or_default();
    let table = app.load_table().await?;
    let q = query.to_lowercase();
    let items: Vec<Ticket> = table
        .tickets
        .iter()
        .filter(|t| !t.id().is_empty())
        .filter(|t| {
            [Col::TicketId, Col::Title, Col::Body]
                .iter()
                .any(|&c| t.get(c).to_lowercase().contains(&q))
        })
        .cloned()
        .collect();
    send_list(app, i, format!("検索: {query}"), items).await
}

// ---------- /sync ----------

async fn sync(app: &App, i: &Interaction, sub: &str, opts: &[CommandDataOption]) -> Result<()> {
    if matches!(sub, "all" | "tags" | "members")
        && !is_admin(i.member.as_ref().and_then(|m| m.permissions))
    {
        return respond(app, i, &interact::ephemeral("このコマンドは管理者のみ実行できます")).await;
    }
    respond(app, i, &defer_ephemeral()).await?;
    match sub {
        "ticket" => {
            let id = str_opt(opts, "id").unwrap_or_default();
            let reply = match ops::sync_one(app, &id).await {
                Ok(()) => format!("{id} を同期しました"),
                Err(e) => format!("同期に失敗しました: {e:#}"),
            };
            edit_reply(app, i, &reply).await
        }
        "all" => {
            app.reload_masters().await?;
            crate::render::sync_forum_tags(app, false).await?;
            let n = ops::sync_all(app, true).await?;
            edit_reply(app, i, &format!("{n} 件を同期しました")).await
        }
        "check" => sync_check(app, i).await,
        "tags" => {
            let report = ops::sync_tags(app).await?;
            let text = if report.is_empty() {
                "変更はありませんでした".to_owned()
            } else {
                report.join("\n")
            };
            edit_reply(app, i, &text).await
        }
        "members" => sync_members(app, i).await,
        _ => Ok(()),
    }
}

/// /sync check: マスタ不整合・必須欠落・ID重複を検査する
async fn sync_check(app: &App, i: &Interaction) -> Result<()> {
    app.reload_masters().await?;
    let issues = ops::check(app).await?;
    if issues.is_empty() {
        return edit_reply(app, i, "問題は見つかりませんでした ✅").await;
    }
    let mut chunks = vec![format!("{} 件の問題があります\n", issues.len())];
    for issue in issues {
        let cur = chunks.last_mut().unwrap();
        if cur.len() + issue.len() > 1900 {
            chunks.push(String::new());
        }
        let cur = chunks.last_mut().unwrap();
        cur.push_str("- ");
        cur.push_str(&issue);
        cur.push('\n');
    }
    edit_reply(app, i, &chunks[0]).await?;
    for chunk in &chunks[1..] {
        followup(app, i, chunk).await?;
    }
    Ok(())
}

/// /sync members: サーバー参加者を担当者マスタに取り込む
async fn sync_members(app: &App, i: &Interaction) -> Result<()> {
    let mut added = Vec::new();
    let mut after = None;
    loop {
        let mut req = app.http.guild_members(app.guild_id()).limit(1000);
        if let Some(id) = after {
            req = req.after(id);
        }
        let members = req.await?.models().await?;
        let Some(last) = members.last() else { break };
        after = Some(last.user.id);
        let n = members.len();
        for m in members.iter().filter(|m| !m.user.bot) {
            let name = app
                .sheet_name_of(
                    m.user.id.get(),
                    &m.user.name,
                    display_name(m.nick.as_deref(), &m.user),
                )
                .await;
            if ops::add_member(app, m.user.id.get(), &name).await? {
                added.push(name);
            }
        }
        if n < 1000 {
            break;
        }
    }
    let text = if added.is_empty() {
        "追加された担当者はいません".to_owned()
    } else {
        format!("追加: {}", added.join(", "))
    };
    edit_reply(app, i, &text).await
}
