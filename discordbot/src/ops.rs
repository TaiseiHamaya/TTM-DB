//! チケット操作の本体。コマンド・ボタン・イベント・スプシ (GAS) からの同期で呼ばれる。

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate};
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::http::attachment::Attachment;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, UserMarker};

use crate::app::App;
use crate::config::Col;
use crate::masters::{Masters, StatusRole, can_transition};
use crate::render::{self, mention_only};
use crate::store::{SchemaError, Table, Ticket};

const FAIL_REACTION: RequestReactionType<'static> = RequestReactionType::Unicode { name: "⚠" };

pub struct NewTicket {
    pub title: String,
    pub body: String,
    pub category: String,
    pub assignee_name: String,
    pub assignee_id: u64,
    pub reporter_name: String,
    pub reporter_id: u64,
    pub priority: String,
    pub due_date: String,
    pub parent_id: Option<String>,
    /// 発行フォームで添付された画像
    pub images: Vec<NewImage>,
}

/// 発行時の添付画像。Post に再投稿し、失敗したら元の URL をそのまま記録する
pub struct NewImage {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub source_url: String,
}

pub struct Created {
    pub ticket_id: String,
    pub url: String,
    pub warnings: Vec<String>,
}

/// "2026-10-10" / "2026/10/10" / "20261010" を YYYY-MM-DD に正規化
pub fn normalize_date(s: &str) -> Option<String> {
    let s = s.trim();
    ["%Y-%m-%d", "%Y/%m/%d", "%Y%m%d", "%Y-%m-%d %H:%M:%S"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(s, f).ok())
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// /ticket create の本体
pub async fn create_ticket(app: &App, input: NewTicket) -> Result<Created> {
    app.ensure_running().await?;
    let m = app.masters.read().await.clone();
    if !m.categories.contains(&input.category) {
        bail!("種類「{}」は masters にありません", input.category);
    }
    if !m.priorities.contains(&input.priority) {
        bail!("優先度「{}」は masters にありません", input.priority);
    }
    let due = normalize_date(&input.due_date).ok_or_else(|| {
        anyhow!(
            "期限「{}」は日付形式 (YYYY-MM-DD) ではありません",
            input.due_date
        )
    })?;
    if input.title.trim().is_empty() || input.body.trim().is_empty() {
        bail!("タイトルと詳細は必須です");
    }

    // 担当者・発行者が masters に未登録ならサーバー参加者として追記する。
    // シートには名前だけを書くので、名前から Discord ID を引けるようにしておく
    for (id, name) in [
        (input.assignee_id, &input.assignee_name),
        (input.reporter_id, &input.reporter_name),
    ] {
        if let Err(e) = add_member(app, id, name).await {
            tracing::warn!(error = %e, user_id = id, "masters への参加者追加に失敗");
        }
    }
    let m = app.masters.read().await.clone();

    let _guard = app.write_lock.lock().await;
    let table = app.load_table().await?;
    let parent_id = match input
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(p) => Some(
            table
                .find(p)
                .with_context(|| format!("親チケット {p} が見つかりません"))?
                .id()
                .to_owned(),
        ),
        None => None,
    };

    // 発番: 再読込した最大番号 + 1 で追記し、追記後にユニーク確認する
    let mut ticket_id = table.next_id(&app.cfg);
    let now = app.cfg.now();
    let initial = m.status_by_role(StatusRole::Initial).name.clone();
    let values = vec![
        (Col::TicketId, ticket_id.clone()),
        (Col::Title, input.title.trim().to_owned()),
        (Col::Body, input.body.trim().to_owned()),
        (Col::Status, initial),
        (Col::Category, input.category.clone()),
        (Col::Assignee, input.assignee_name.clone()),
        (Col::Reporter, input.reporter_name.clone()),
        (Col::Priority, input.priority.clone()),
        (Col::DueDate, due),
        (Col::ParentId, parent_id.clone().unwrap_or_default()),
        (Col::CreatedAt, now.clone()),
        (Col::UpdatedAt, now.clone()),
    ];
    let row = app.store().append(&table, &values).await?;
    tracing::info!(ticket_id = %ticket_id, sheets_row = row, "チケットを Sheets に追記");

    let mut warnings = Vec::new();
    let table = app.load_table().await?;
    let first_row = table
        .tickets
        .iter()
        .filter(|t| t.id() == ticket_id)
        .map(|t| t.row)
        .min();
    if first_row.is_some_and(|r| r != row) {
        let new_id = format!("{ticket_id}-{row}");
        app.store()
            .update(&table, row, &ticket_id, &[(Col::TicketId, new_id.clone())])
            .await?;
        let msg = format!("ID {ticket_id} が同時発番で重複したため {new_id} に変更しました");
        tracing::warn!(ticket_id = %new_id, sheets_row = row, "{msg}");
        warnings.push(msg);
        ticket_id = new_id;
    }

    let table = app.load_table().await?;
    let ticket = table
        .find(&ticket_id)
        .cloned()
        .context("追記したチケットを再読込できません")?;
    let (post_id, url) = create_post(app, &table, &ticket, &m).await?;
    if !input.images.is_empty()
        && let Err(e) = attach_images(app, &ticket, post_id, input.images).await
    {
        tracing::warn!(error = %e, ticket_id = %ticket_id, "添付画像の登録に失敗");
        warnings.push(format!("添付画像の登録に失敗しました: {e:#}"));
    }

    // 親 Post へ子のリンクを投稿
    if let Some(parent) = parent_id.as_deref().and_then(|p| table.find(p))
        && let Some(parent_post) = parent.post_id()
    {
        let text = format!(
            "子チケットが作成されました: [{} {}]({url})",
            ticket_id,
            render::truncate(ticket.get(Col::Title), 80)
        );
        if let Err(e) = app
            .http
            .create_message(Id::new(parent_post))
            .content(&text)
            .await
        {
            tracing::warn!(error = %e, ticket_id = %parent.id(), "親 Post への投稿に失敗");
        }
        // 親の Embed の子チケット欄を更新
        let table = app.load_table().await?;
        if let Some(parent) = table.find(parent.id()) {
            let _ = push_ticket(app, &table, parent).await;
        }
    }
    tracing::info!(ticket_id = %ticket_id, discord_post_id = post_id, sheets_row = row, "チケット発行完了");
    Ok(Created {
        ticket_id,
        url,
        warnings,
    })
}

/// 発行フォームの画像を Post に投稿し、その URL を image_urls に記録する。
/// 再投稿に失敗した画像は元の URL を記録する
async fn attach_images(app: &App, t: &Ticket, post_id: u64, images: Vec<NewImage>) -> Result<()> {
    let files: Vec<Attachment> = images
        .iter()
        .enumerate()
        .map(|(i, img)| Attachment::from_bytes(img.filename.clone(), img.bytes.clone(), i as u64))
        .collect();
    let urls: Vec<String> = match app
        .http
        .create_message(Id::new(post_id))
        .content("発行時に添付された画像")
        .attachments(&files)
        .await
    {
        Ok(res) => res.model().await?.attachments.into_iter().map(|a| a.url).collect(),
        Err(e) => {
            tracing::warn!(error = %e, ticket_id = %t.id(), "画像の再投稿に失敗。元の URL を記録");
            images.into_iter().map(|i| i.source_url).collect()
        }
    };
    add_images_locked(app, post_id, urls).await?;
    Ok(())
}

/// Forum Post を作り、post_id と URL を Sheets に書き戻す
async fn create_post(app: &App, table: &Table, t: &Ticket, m: &Masters) -> Result<(u64, String)> {
    let mention = mention_only(render::assignee_user(t, m, &app.cfg));
    let title = render::post_title(t);
    let content = render::starter_content(t, m, &app.cfg);
    let embeds = [render::embed(t, table, m, &app.cfg)];
    let components = render::components(t, table, m);
    let tags = render::applied_tags(app, t).await;
    let post = app
        .http
        .create_forum_thread(app.forum_id(), &title)
        .applied_tags(&tags)
        .message()
        .content(&content)
        .embeds(&embeds)
        .components(&components)
        .allowed_mentions(Some(&mention))
        .await
        .context("Forum Post の作成に失敗しました")?
        .model()
        .await?;
    let post_id = post.channel.id;
    let url = render::post_url(app, post_id);
    app.store()
        .update(
            table,
            t.row,
            t.id(),
            &[
                (Col::DiscordPostId, post_id.to_string()),
                (Col::DiscordUrl, url.clone()),
            ],
        )
        .await?;
    app.http
        .create_message(post_id)
        .content("追加の画像は、このスレッドに投稿すると自動でチケットに登録されます。")
        .await?;

    let mut t = t.clone();
    t.set(Col::DiscordPostId, post_id.to_string());
    t.set(Col::DiscordUrl, url.clone());
    let mut st = app.sync_state.lock().await;
    st.hashes.insert(t.id().to_owned(), t.content_hash());
    st.pushed_at
        .insert(t.id().to_owned(), chrono::Utc::now().timestamp_millis());
    st.save();
    Ok((post_id.get(), url))
}

/// Sheets の内容を Discord に反映する (タイトル・タグ・Locked・先頭 Embed)。
/// Post が無ければ作成する。失敗時は先頭メッセージに同期失敗リアクションを付ける
pub async fn push_ticket(app: &App, table: &Table, t: &Ticket) -> Result<()> {
    let m = app.masters.read().await.clone();
    let result = match t.post_id() {
        None => create_post(app, table, t, &m).await.map(|_| ()),
        Some(post_id) => update_post(app, table, t, &m, post_id).await,
    };
    let mut st = app.sync_state.lock().await;
    match &result {
        Ok(()) => {
            st.hashes.insert(t.id().to_owned(), t.content_hash());
            st.pushed_at
                .insert(t.id().to_owned(), chrono::Utc::now().timestamp_millis());
            if st.failed.remove(t.id())
                && let Some(post_id) = t.post_id()
            {
                let _ = app
                    .http
                    .delete_current_user_reaction(Id::new(post_id), Id::new(post_id), &FAIL_REACTION)
                    .await;
            }
        }
        Err(e) => {
            tracing::error!(
                ticket_id = %t.id(),
                discord_post_id = t.post_id().unwrap_or(0),
                sheets_row = t.row,
                error = %e,
                "Discord への同期に失敗"
            );
            if let Some(post_id) = t.post_id() {
                let _ = app
                    .http
                    .create_reaction(Id::new(post_id), Id::new(post_id), &FAIL_REACTION)
                    .await;
                if st.failed.insert(t.id().to_owned()) {
                    let text = format!(
                        "⚠ 同期に失敗しました。`/sync ticket id:{}` で再試行してください。",
                        t.id()
                    );
                    let _ = app.http.create_message(Id::new(post_id)).content(&text).await;
                }
            }
        }
    }
    st.save();
    result
}

async fn update_post(
    app: &App,
    table: &Table,
    t: &Ticket,
    m: &Masters,
    post_id: u64,
) -> Result<()> {
    let ch_id: Id<ChannelMarker> = Id::new(post_id);
    let thread = app
        .http
        .channel(ch_id)
        .await
        .context("Post が見つかりません")?
        .model()
        .await?;
    let title = render::post_title(t);
    let tags = render::applied_tags(app, t).await;
    let lock = m.role_of(t.get(Col::Status)).is_some_and(StatusRole::locks);
    let meta = thread.thread_metadata.as_ref();
    let archived = meta.is_some_and(|x| x.archived);
    let locked = meta.is_some_and(|x| x.locked);

    let mut edit = app.http.update_thread(ch_id);
    let mut need = false;
    // スレッド名変更は Discord 側のレート制限が厳しいので差分があるときだけ
    if thread.name.as_deref() != Some(title.as_str()) {
        edit = edit.name(&title);
        need = true;
    }
    let mut cur_tags = thread.applied_tags.clone().unwrap_or_default();
    let mut new_tags = tags.clone();
    cur_tags.sort();
    new_tags.sort();
    if cur_tags != new_tags {
        edit = edit.applied_tags(Some(&tags));
        need = true;
    }
    // アーカイブされない運用。編集のため必ずアンアーカイブする
    if archived {
        edit = edit.archived(false);
        need = true;
    }
    if locked != lock {
        edit = edit.locked(lock);
        need = true;
    }
    if need {
        edit.await?;
    }

    let mention = mention_only(render::assignee_user(t, m, &app.cfg));
    let content = render::starter_content(t, m, &app.cfg);
    let embeds = [render::embed(t, table, m, &app.cfg)];
    let components = render::components(t, table, m);
    app.http
        .update_message(ch_id, Id::new(post_id))
        .content(Some(&content))
        .embeds(Some(&embeds))
        .components(Some(&components))
        .allowed_mentions(Some(&mention))
        .await?;
    Ok(())
}

pub enum StatusOutcome {
    Changed { from: String, to: String },
}

/// 進行度の変更。権限チェック → (LWW) → Sheets 更新 → Discord 反映 → 担当者宛通知
pub async fn change_status(
    app: &App,
    ticket_id: &str,
    target: StatusRole,
    actor: Id<UserMarker>,
    actor_is_admin: bool,
    seen_updated_at: Option<String>,
) -> Result<StatusOutcome> {
    app.ensure_running().await?;
    let m = app.masters.read().await.clone();
    let _guard = app.write_lock.lock().await;
    let table = app.load_table().await?;
    let t = table
        .find(ticket_id)
        .cloned()
        .with_context(|| format!("チケット {ticket_id} が見つかりません"))?;

    let actor_id = actor.get();
    let assignee = render::assignee_user(&t, &m, &app.cfg).map(|u| u.get());
    let reporter = render::reporter_user(&t, &m, &app.cfg).map(|u| u.get());
    if !actor_is_admin && assignee != Some(actor_id) && reporter != Some(actor_id) {
        bail!("進行度を変更できるのは担当者か発行者のみです");
    }

    // Last-Write-Wins: 表示中の内容より Sheets が新しければ Discord 側操作を拒否
    if let Some(seen) = seen_updated_at
        && is_newer(t.get(Col::UpdatedAt), &seen)
    {
        drop(_guard);
        let _ = push_ticket(app, &table, &t).await;
        bail!(
            "スプシ側で先に更新されていたため操作を取り消しました。表示を再読込したので内容を確認してもう一度操作してください"
        );
    }

    let from_name = t.get(Col::Status).to_owned();
    let from = m.role_of(&from_name).with_context(|| {
        format!(
            "現在の進行度「{from_name}」が masters にありません。`/sync check` で確認してください"
        )
    })?;
    let to_name = m.status_by_role(target).name.clone();
    if !can_transition(from, target) {
        bail!("「{from_name}」から「{to_name}」には変更できません");
    }

    let now = app.cfg.now();
    let mut changes = vec![(Col::Status, to_name.clone()), (Col::UpdatedAt, now)];
    // 着手日は最初に着手中にした日を残す (差戻しでは上書きしない)
    if target == StatusRole::InProgress && t.get(Col::StartedAt).is_empty() {
        changes.push((Col::StartedAt, app.cfg.today()));
    }
    // 完了日は完了にした日。差戻しで空に戻し、再度の完了で付け直す
    if target == StatusRole::Done {
        changes.push((Col::CompletedAt, app.cfg.today()));
    } else if from == StatusRole::Done && !t.get(Col::CompletedAt).is_empty() {
        changes.push((Col::CompletedAt, String::new()));
    }
    app.store().update(&table, t.row, t.id(), &changes).await?;
    let mut updated = t.clone();
    for (col, v) in changes {
        updated.set(col, v);
    }
    tracing::info!(
        ticket_id = %t.id(),
        discord_post_id = t.post_id().unwrap_or(0),
        sheets_row = t.row,
        from = %from_name,
        to = %to_name,
        "進行度を変更"
    );
    push_ticket(app, &table, &updated).await?;

    if let Some(post_id) = updated.post_id() {
        let who = assignee
            .map(|id| format!("<@{id}> "))
            .unwrap_or_else(|| format!("{} さん ", updated.get(Col::Assignee)));
        let text = format!(
            "{who}進行度が「{from_name}」→「{to_name}」に変更されました (by <@{actor_id}>)"
        );
        let mention = mention_only(assignee.map(Id::new));
        app.http
            .create_message(Id::new(post_id))
            .content(&text)
            .allowed_mentions(Some(&mention))
            .await?;
    }
    Ok(StatusOutcome::Changed {
        from: from_name,
        to: to_name,
    })
}

/// a が b より新しいか (ISO8601 として比較。解釈できなければ文字列の不一致で判定)
fn is_newer(a: &str, b: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(a),
        DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => x > y,
        _ => !a.is_empty() && a != b,
    }
}

/// Post 内に投稿された画像を image_urls に追記する
pub async fn add_images(app: &App, post_id: u64, urls: Vec<String>) -> Result<Option<String>> {
    app.ensure_running().await?;
    let _guard = app.write_lock.lock().await;
    add_images_locked(app, post_id, urls).await
}

/// add_images の本体。呼び出し側で write_lock を取得済みであること
async fn add_images_locked(app: &App, post_id: u64, urls: Vec<String>) -> Result<Option<String>> {
    let table = app.load_table().await?;
    let Some(t) = table.find_by_post(post_id).cloned() else {
        return Ok(None);
    };
    let mut all: Vec<String> = t.image_urls().into_iter().map(str::to_owned).collect();
    all.extend(urls);
    let joined = all.join(",");
    let now = app.cfg.now();
    app.store()
        .update(
            &table,
            t.row,
            t.id(),
            &[
                (Col::ImageUrls, joined.clone()),
                (Col::UpdatedAt, now.clone()),
            ],
        )
        .await?;
    let mut updated = t.clone();
    updated.set(Col::ImageUrls, joined);
    updated.set(Col::UpdatedAt, now);
    tracing::info!(ticket_id = %t.id(), discord_post_id = post_id, sheets_row = t.row, "画像を登録");
    push_ticket(app, &table, &updated).await?;
    Ok(Some(t.id().to_owned()))
}

/// スキーマ検査結果を halted に反映し、状態が変わったら警告を投稿する
async fn set_halted(app: &App, reason: Option<String>) {
    let mut h = app.halted.write().await;
    if *h == reason {
        return;
    }
    let msg = match &reason {
        Some(r) => {
            format!("🚨 TTM-DB: {r}\n同期を停止しました。列を元に戻すと自動で再開します。")
        }
        None => "✅ TTM-DB: スプシの構成が正常に戻ったため同期を再開しました。".to_owned(),
    };
    match &reason {
        Some(r) => tracing::error!(reason = %r, "同期停止"),
        None => tracing::info!("同期再開"),
    }
    *h = reason;
    drop(h);
    alert(app, msg).await;
}

pub async fn alert(app: &App, msg: String) {
    if let Some(ch) = app.cfg.discord.alert_channel_id
        && let Err(e) = app.http.create_message(Id::new(ch)).content(&msg).await
    {
        tracing::warn!(error = %e, "警告の投稿に失敗");
    }
}

/// スキーマ検査の結果を同期停止状態に反映する。スキーマ破壊なら None
async fn check_schema(
    app: &App,
    table: Result<Table>,
) -> Result<Option<Table>> {
    match table {
        Ok(t) => {
            set_halted(app, None).await;
            Ok(Some(t))
        }
        Err(e) => match e.downcast_ref::<SchemaError>() {
            Some(se) => {
                set_halted(app, Some(se.to_string())).await;
                Ok(None)
            }
            None => Err(e),
        },
    }
}

/// テーブルを読み込み、スキーマ破壊なら同期停止にする
pub async fn load_checked(app: &App) -> Result<Option<Table>> {
    let table = app.load_table().await;
    check_schema(app, table).await
}

/// スプシの GAS が Webhook で送ってくる内容 (各シートの表示値)
#[derive(Debug, serde::Deserialize)]
pub struct Snapshot {
    /// GAS がシートを読んだ時刻 (UNIX ミリ秒)
    pub read_at: i64,
    /// tickets シート全体 (1行目がヘッダ)
    pub tickets: Option<Vec<Vec<String>>>,
    /// masters シートの A:C
    pub masters: Option<Vec<Vec<String>>>,
}

/// GAS から届いたシート内容を Discord に反映する (Bot から Sheets は読まない)
pub async fn apply_snapshot(app: &App, snap: Snapshot) -> Result<usize> {
    let _guard = app.write_lock.lock().await;
    {
        let mut st = app.sync_state.lock().await;
        if snap.read_at <= st.last_snapshot_at {
            tracing::info!(read_at = snap.read_at, "古いスプシ送信内容のため無視");
            return Ok(0);
        }
        st.last_snapshot_at = snap.read_at;
    }
    if let Some(rows) = &snap.masters {
        let m = Masters::parse(rows, &app.cfg).context("masters の内容が不正です")?;
        *app.masters.write().await = m;
        if let Err(e) = render::sync_forum_tags(app, false).await {
            tracing::error!(error = %e, "Forum タグの同期に失敗");
        }
    }
    let Some(rows) = &snap.tickets else {
        return Ok(0);
    };
    let Some(table) = check_schema(app, Table::from_rows(rows, &app.cfg)).await? else {
        return Ok(0);
    };
    app.cache_table(&table).await;
    sync_table(app, &table, false, Some(snap.read_at)).await
}

/// Sheets 全行を読み直して Discord に反映する (/sync all など手動用)
pub async fn sync_all(app: &App, force: bool) -> Result<usize> {
    let _guard = app.write_lock.lock().await;
    let Some(table) = load_checked(app).await? else {
        return Ok(0);
    };
    sync_table(app, &table, force, None).await
}

/// 前回反映時から内容が変わったチケットを Discord に反映する。
/// `read_at` があれば、その時刻より後に Bot が反映したチケットは (送信内容の方が古いので) 飛ばす
async fn sync_table(
    app: &App,
    table: &Table,
    force: bool,
    read_at: Option<i64>,
) -> Result<usize> {
    let fresh = {
        let mut st = app.sync_state.lock().await;
        std::mem::take(&mut st.fresh)
    };
    let mut seen = std::collections::HashSet::new();
    let mut pushed = 0;
    for t in &table.tickets {
        if t.id().is_empty() || !seen.insert(t.id().to_lowercase()) {
            continue;
        }
        let hash = t.content_hash();
        let (last, pushed_at) = {
            let st = app.sync_state.lock().await;
            (
                st.hashes.get(t.id()).copied(),
                st.pushed_at.get(t.id()).copied(),
            )
        };
        if let (Some(read_at), Some(pushed_at)) = (read_at, pushed_at)
            && pushed_at > read_at
        {
            continue;
        }
        let need =
            force || t.post_id().is_none() || (last != Some(hash) && !(fresh && last.is_none()));
        if !need {
            if last.is_none() {
                app.sync_state
                    .lock()
                    .await
                    .hashes
                    .insert(t.id().to_owned(), hash);
            }
            continue;
        }
        if push_ticket(app, table, t).await.is_ok() {
            pushed += 1;
        }
    }
    app.sync_state.lock().await.save();
    if pushed > 0 {
        tracing::info!(count = pushed, "Sheets の変更を Discord に反映");
    }
    Ok(pushed)
}

/// /sync ticket: 指定チケットを即時反映
pub async fn sync_one(app: &App, ticket_id: &str) -> Result<()> {
    let _guard = app.write_lock.lock().await;
    let Some(table) = load_checked(app).await? else {
        bail!("スプシの構成に問題があるため同期を停止中です");
    };
    let t = table
        .find(ticket_id)
        .with_context(|| format!("チケット {ticket_id} が見つかりません"))?;
    push_ticket(app, &table, t).await
}

/// /sync check: マスタ不整合と必須欠落を検査する
pub async fn check(app: &App) -> Result<Vec<String>> {
    let m = app.masters.read().await.clone();
    let table = app.load_table().await?;
    let mut issues = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    for t in &table.tickets {
        let label = if t.id().is_empty() {
            format!("{}行目", t.row)
        } else {
            format!("{} ({}行目)", t.id(), t.row)
        };
        for col in app.cfg.required_cols() {
            // post_id と URL は Bot が後から書き込む
            if matches!(col, Col::DiscordPostId | Col::DiscordUrl) {
                continue;
            }
            if t.get(col).is_empty() {
                issues.push(format!("{label}: 必須列 {} が空です", app.cfg.header(col)));
            }
        }
        let check_in = |col: Col, list: &[String], issues: &mut Vec<String>| {
            let v = t.get(col);
            if !v.is_empty() && !list.iter().any(|x| x == v) {
                issues.push(format!(
                    "{label}: {}「{v}」は masters にありません",
                    app.cfg.header(col)
                ));
            }
        };
        let statuses: Vec<String> = m.statuses.iter().map(|s| s.name.clone()).collect();
        let assignees: Vec<String> = m
            .assignees
            .iter()
            .map(|a| a.name.clone())
            .chain(app.cfg.members.values().cloned())
            .collect();
        check_in(Col::Status, &statuses, &mut issues);
        check_in(Col::Category, &m.categories, &mut issues);
        check_in(Col::Priority, &m.priorities, &mut issues);
        check_in(Col::Assignee, &assignees, &mut issues);
        check_in(Col::Reporter, &assignees, &mut issues);
        let due = t.get(Col::DueDate);
        if !due.is_empty() && normalize_date(due).is_none() {
            issues.push(format!("{label}: 期限「{due}」が日付形式ではありません"));
        }
        let parent = t.get(Col::ParentId);
        if !parent.is_empty() && table.find(parent).is_none() {
            issues.push(format!("{label}: 親チケット {parent} が存在しません"));
        }
        if !t.id().is_empty() {
            *ids.entry(t.id().to_lowercase()).or_default() += 1;
        }
    }
    for (id, n) in ids {
        if n > 1 {
            issues.push(format!("ticket_id {id} が {n} 行で重複しています"));
        }
    }
    // 設定の表示名対応と masters の担当者名が食い違っていないか
    for a in &m.assignees {
        if let Some(id) = a.discord_id
            && let Some(name) = app.cfg.member_name(id, "")
            && name != a.name
        {
            issues.push(format!(
                "masters の担当者「{}」(ID {id}) が設定の表示名「{name}」と異なります",
                a.name
            ));
        }
    }
    Ok(issues)
}

/// /sync tags: masters の rename を Forum タグと tickets の値に反映し、不足タグを追加する
pub async fn sync_tags(app: &App) -> Result<Vec<String>> {
    app.reload_masters().await?;
    let mut report = render::sync_forum_tags(app, true).await?;
    let m = app.masters.read().await.clone();
    if m.renames.is_empty() {
        return Ok(report);
    }
    let _guard = app.write_lock.lock().await;
    let table = app.load_table().await?;
    let now = app.cfg.now();
    for t in &table.tickets {
        let mut changes = Vec::new();
        for col in [Col::Status, Col::Category, Col::Assignee, Col::Priority] {
            if let Some((_, new)) = m.renames.iter().find(|(old, _)| old == t.get(col)) {
                changes.push((col, new.clone()));
            }
        }
        if changes.is_empty() {
            continue;
        }
        changes.push((Col::UpdatedAt, now.clone()));
        app.store().update(&table, t.row, t.id(), &changes).await?;
        report.push(format!("{}: 値を新名に移行", t.id()));
    }
    drop(_guard);
    sync_all(app, false).await?;
    Ok(report)
}

/// サーバー参加者を masters に追記し、タグを追加する
pub async fn add_member(
    app: &App,
    user_id: u64,
    name: &str,
) -> Result<bool> {
    if app.masters.read().await.assignee_by_id(user_id).is_some() {
        return Ok(false);
    }
    Masters::append_assignee(&app.sheets, &app.cfg, name, user_id).await?;
    app.reload_masters().await?;
    render::sync_forum_tags(app, false).await?;
    tracing::info!(user_id, name, "担当者を masters に追加");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(normalize_date("2026-10-10").as_deref(), Some("2026-10-10"));
        assert_eq!(normalize_date("2026/1/5").as_deref(), Some("2026-01-05"));
        assert_eq!(normalize_date("20261010").as_deref(), Some("2026-10-10"));
        assert_eq!(normalize_date("2026-13-01"), None);
        assert_eq!(normalize_date("明日"), None);
    }

    #[test]
    fn header_on_later_row() {
        let mut cfg =
            crate::config::Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))
                .unwrap();
        cfg.sheets.header_row = 3;
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let header = s(&[
            "ticket_id", "discord_post_id", "title", "body", "status", "category", "assignee",
            "reporter", "priority", "due_date", "created_at", "updated_at", "discord_url",
            "started_at",
        ]);
        let rows = vec![
            s(&["チケット台帳"]),
            Vec::new(),
            header,
            s(&["T-0001", "", "件名", "詳細", "未着手", "バグ・違和感", "田中", "佐藤", "高"]),
        ];
        let table = Table::from_rows(&rows, &cfg).unwrap();
        assert_eq!(table.tickets.len(), 1);
        // 行番号はシート上の行番号のまま (更新時のセル位置に使う)
        assert_eq!(table.tickets[0].row, 4);
        assert_eq!(table.next_id(&cfg), "T-0002");

        // 書式設定済みの空行 (未知列のチェックボックス初期値だけがある行) はチケットにしない。
        // 期限・着手日は表示形式に依らず YYYY-MM-DD で読む
        let mut rows = rows;
        rows[3].resize(13, String::new());
        rows[3][9] = "2026/10/9".into();
        rows[3].push("2026/10/8".into());
        let mut blank = vec![String::new(); 14];
        blank.push("FALSE".into());
        rows.push(blank);
        let table = Table::from_rows(&rows, &cfg).unwrap();
        assert_eq!(table.tickets.len(), 1);
        assert_eq!(table.tickets[0].get(Col::DueDate), "2026-10-09");
        assert_eq!(table.tickets[0].get(Col::StartedAt), "2026-10-08");

        // ヘッダ行の指定がずれていればスキーマエラー
        cfg.sheets.header_row = 1;
        assert!(Table::from_rows(&rows, &cfg).is_err());
    }

    #[test]
    fn snapshot_from_gas() {
        let cfg = crate::config::Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))
            .unwrap();
        let json = r#"{
            "read_at": 1791000000000,
            "tickets": [
                ["memo", "ticket_id", "discord_post_id", "title", "body", "status", "category",
                 "assignee", "reporter", "priority", "due_date", "created_at", "updated_at",
                 "discord_url"],
                ["x", "T-0001", "", "件名", "詳細", "未着手", "バグ", "田中", "佐藤", "高",
                 "2026-10-10", "", "", ""],
                ["", "", "", "", "", "", "", "", "", "", "", "", "", ""]
            ],
            "masters": null
        }"#;
        let snap: Snapshot = serde_json::from_str(json).unwrap();
        assert!(snap.masters.is_none());
        let table = Table::from_rows(snap.tickets.as_ref().unwrap(), &cfg).unwrap();
        assert_eq!(table.tickets.len(), 1);
        assert_eq!(table.tickets[0].row, 2);
        assert_eq!(table.tickets[0].get(Col::Title), "件名");

        // 必須列が無ければスキーマエラー
        let broken = vec![vec!["ticket_id".to_owned()]];
        let err = Table::from_rows(&broken, &cfg).unwrap_err();
        assert!(err.downcast_ref::<SchemaError>().is_some());
    }

    #[test]
    fn newer() {
        assert!(is_newer(
            "2026-10-02T12:00:01+09:00",
            "2026-10-02T12:00:00+09:00"
        ));
        assert!(!is_newer(
            "2026-10-02T12:00:00+09:00",
            "2026-10-02T12:00:00+09:00"
        ));
        assert!(!is_newer(
            "2026-10-02T03:00:00+00:00",
            "2026-10-02T12:00:00+09:00"
        ));
    }
}
