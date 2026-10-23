//! 環境変数 (秘匿情報・ID) と config.toml (シート名・列名・周期など) の読み込み。
//! 列名やマスタのキー名はコードに直書きせず、ここ経由で参照する。

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// 環境変数から読む値。リポジトリに含めないもの。
#[derive(Debug, Clone)]
pub struct Env {
    pub discord_token: String,
    pub guild_id: u64,
    pub forum_channel_id: u64,
    pub spreadsheet_id: String,
    /// Service Account JSON のファイルパス、または JSON 文字列そのもの
    pub google_sa_json: String,
    pub config_path: String,
}

impl Env {
    pub fn load() -> Result<Self> {
        fn var(name: &str) -> Result<String> {
            std::env::var(name).with_context(|| format!("環境変数 {name} が未設定です"))
        }
        fn id(name: &str) -> Result<u64> {
            var(name)?
                .trim()
                .parse()
                .with_context(|| format!("環境変数 {name} は数値IDである必要があります"))
        }
        Ok(Self {
            discord_token: var("DISCORD_TOKEN")?,
            guild_id: id("GUILD_ID")?,
            forum_channel_id: id("FORUM_CHANNEL_ID")?,
            spreadsheet_id: var("SPREADSHEET_ID")?,
            google_sa_json: var("GOOGLE_SA_JSON")?,
            config_path: std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".into()),
        })
    }
}

/// tickets シートの論理列。Bot はこの論理名で扱い、実際のヘッダ名は config.toml で対応付ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Col {
    TicketId,
    DiscordPostId,
    Title,
    Body,
    Status,
    Category,
    Assignee,
    Reporter,
    Priority,
    DueDate,
    StartedAt,
    CompletedAt,
    ParentId,
    ImageUrls,
    CreatedAt,
    UpdatedAt,
    DiscordUrl,
}

impl Col {
    pub const ALL: [Col; 17] = [
        Col::TicketId,
        Col::DiscordPostId,
        Col::Title,
        Col::Body,
        Col::Status,
        Col::Category,
        Col::Assignee,
        Col::Reporter,
        Col::Priority,
        Col::DueDate,
        Col::StartedAt,
        Col::CompletedAt,
        Col::ParentId,
        Col::ImageUrls,
        Col::CreatedAt,
        Col::UpdatedAt,
        Col::DiscordUrl,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Col::TicketId => "ticket_id",
            Col::DiscordPostId => "discord_post_id",
            Col::Title => "title",
            Col::Body => "body",
            Col::Status => "status",
            Col::Category => "category",
            Col::Assignee => "assignee",
            Col::Reporter => "reporter",
            Col::Priority => "priority",
            Col::DueDate => "due_date",
            Col::StartedAt => "started_at",
            Col::CompletedAt => "completed_at",
            Col::ParentId => "parent_id",
            Col::ImageUrls => "image_urls",
            Col::CreatedAt => "created_at",
            Col::UpdatedAt => "updated_at",
            Col::DiscordUrl => "discord_url",
        }
    }

    fn from_key(key: &str) -> Option<Col> {
        Col::ALL.into_iter().find(|c| c.key() == key)
    }

    /// 廃止した論理列名なら、その案内
    fn removed(key: &str) -> Option<&'static str> {
        match key {
            "assignee_id" => Some("担当者は assignee 列の名前だけで記録します"),
            "reporter_id" => Some("発行者は reporter 列に名前で記録します"),
            _ => None,
        }
    }

    /// 論理列名の検証 (未知・廃止ならエラー)
    fn parse(key: &str, source: &str) -> Result<Col> {
        if let Some(c) = Col::from_key(key) {
            return Ok(c);
        }
        match Col::removed(key) {
            Some(hint) => bail!("{source} の論理列名 {key} は廃止しました。{hint}"),
            None => bail!("{source} の論理列名 {key} は未知です"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub sheets: SheetsConfig,
    pub columns: ColumnsConfig,
    pub masters: MastersConfig,
    pub sync: SyncConfig,
    pub ticket: TicketConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub tags: TagsConfig,
    /// Discord ユーザー -> スプシ上の表示名。キーは Discord ユーザーID またはユーザー名
    #[serde(default)]
    pub members: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SheetsConfig {
    pub tickets_sheet: String,
    pub masters_sheet: String,
    /// tickets シートのヘッダの行番号 (1始まり)。これより上の行は無視し、下の行をチケットとして読む
    #[serde(default = "default_header_row")]
    pub header_row: usize,
}

fn default_header_row() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnsConfig {
    /// 論理名 -> スプシのヘッダ名。未指定の列は論理名をそのままヘッダ名とする
    #[serde(default)]
    pub names: HashMap<String, String>,
    /// 必須列 (論理名)。起動時・スプシからの同期時に存在を検査する
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MastersConfig {
    pub status_key: String,
    pub category_key: String,
    pub assignee_key: String,
    pub priority_key: String,
    pub rename_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncConfig {
    pub state_file: String,
    /// スプシ編集の通知を受け取る Pub/Sub サブスクリプション
    /// (projects/<プロジェクトID>/subscriptions/<名前>)。未設定ならスプシ側の編集は反映しない
    pub pubsub_subscription: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TicketConfig {
    pub id_prefix: String,
    pub id_digits: usize,
    /// タイムスタンプのタイムゾーン (UTCからの時間差)
    pub utc_offset_hours: i32,
    pub list_limit: usize,
    /// Forum タグ上限 (Discord 仕様で 20)
    pub max_forum_tags: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiscordConfig {
    /// スキーマ破壊などの警告を投稿するテキストチャンネル。未設定ならログのみ
    pub alert_channel_id: Option<u64>,
}

/// Forum タグ名の接頭辞 (どの要素のタグかを示す)。タグ名は「接頭辞 + masters の値」になる。
/// 絵文字はタグ名とは別にタグへ付く (空ならなし)
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TagsConfig {
    pub status_prefix: String,
    pub category_prefix: String,
    pub priority_prefix: String,
    pub assignee_prefix: String,
    pub status_emoji: String,
    pub category_emoji: String,
    pub priority_emoji: String,
    pub assignee_emoji: String,
}

impl Default for TagsConfig {
    fn default() -> Self {
        Self {
            status_prefix: "進行度:".into(),
            category_prefix: "種類:".into(),
            priority_prefix: "優先度:".into(),
            assignee_prefix: "担当:".into(),
            status_emoji: "🚦".into(),
            category_emoji: "🧩".into(),
            priority_emoji: "🚩".into(),
            assignee_emoji: "👤".into(),
        }
    }
}

impl TagsConfig {
    /// タグにする列の接頭辞 (タグにしない列は空)
    pub fn prefix(&self, col: Col) -> &str {
        match col {
            Col::Status => &self.status_prefix,
            Col::Category => &self.category_prefix,
            Col::Priority => &self.priority_prefix,
            Col::Assignee => &self.assignee_prefix,
            _ => "",
        }
    }

    /// タグに付ける Unicode 絵文字 (タグにしない列や空設定は None)
    pub fn emoji(&self, col: Col) -> Option<&str> {
        let e = match col {
            Col::Status => &self.status_emoji,
            Col::Category => &self.category_emoji,
            Col::Priority => &self.priority_emoji,
            Col::Assignee => &self.assignee_emoji,
            _ => "",
        };
        (!e.is_empty()).then_some(e)
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("設定ファイル {path} を読めません"))?;
        let mut cfg: Config = toml::from_str(&text).context("config.toml の形式が不正です")?;
        cfg.apply_env(|name| std::env::var(name).ok(), std::env::vars())?;
        if cfg.sheets.header_row == 0 {
            bail!("sheets.header_row は1以上を指定してください");
        }
        if let Some(sub) = &cfg.sync.pubsub_subscription
            && !is_subscription_path(sub)
        {
            bail!(
                "sync.pubsub_subscription「{sub}」は projects/<プロジェクトID>/subscriptions/<名前> 形式で指定してください"
            );
        }
        for key in cfg.columns.names.keys().chain(cfg.columns.required.iter()) {
            Col::parse(key, "config.toml の columns")?;
        }
        for col in Col::ALL {
            cfg.columns
                .names
                .entry(col.key().to_owned())
                .or_insert_with(|| col.key().to_owned());
        }
        Ok(cfg)
    }

    /// 環境変数による上書き (ファイルより優先)
    /// - TTM_TICKETS_SHEET / TTM_MASTERS_SHEET: シート名
    /// - TTM_HEADER_ROW: tickets シートのヘッダの行番号
    /// - TTM_COL_<論理名の大文字> (例: TTM_COL_TITLE=件名): ヘッダ名
    /// - TTM_MEMBERS="<ユーザーID or ユーザー名>=<スプシ表示名>,...": 表示名の対応 (追加・上書き)
    fn apply_env(
        &mut self,
        get: impl Fn(&str) -> Option<String>,
        all: impl Iterator<Item = (String, String)>,
    ) -> Result<()> {
        if let Some(v) = get("TTM_TICKETS_SHEET") {
            self.sheets.tickets_sheet = v;
        }
        if let Some(v) = get("TTM_MASTERS_SHEET") {
            self.sheets.masters_sheet = v;
        }
        if let Some(v) = get("TTM_HEADER_ROW") {
            self.sheets.header_row = v.trim().parse().with_context(|| {
                format!("TTM_HEADER_ROW「{v}」は行番号 (1以上の整数) ではありません")
            })?;
        }
        for (name, value) in all {
            if let Some(key) = name.strip_prefix("TTM_COL_") {
                let key = key.to_lowercase();
                Col::parse(&key, &format!("環境変数 {name}"))?;
                self.columns.names.insert(key, value);
            }
        }
        if let Some(v) = get("TTM_MEMBERS") {
            for pair in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let Some((user, sheet_name)) = pair.split_once('=') else {
                    bail!("TTM_MEMBERS の「{pair}」は <ユーザー>=<表示名> 形式ではありません");
                };
                self.members
                    .insert(user.trim().to_owned(), sheet_name.trim().to_owned());
            }
        }
        Ok(())
    }

    pub fn header(&self, col: Col) -> &str {
        &self.columns.names[col.key()]
    }

    /// Discord ユーザーに対応するスプシ上の表示名 (ID 指定を優先し、次にユーザー名)
    pub fn member_name(&self, user_id: u64, username: &str) -> Option<&str> {
        self.members
            .get(&user_id.to_string())
            .or_else(|| self.members.get(username))
            .map(String::as_str)
    }

    /// スプシ上の表示名から Discord ユーザーID を引く (ID で対応付けたもののみ)
    pub fn member_id_for_name(&self, sheet_name: &str) -> Option<u64> {
        self.members
            .iter()
            .find(|(_, v)| *v == sheet_name)
            .and_then(|(k, _)| k.parse().ok())
    }

    pub fn required_cols(&self) -> Vec<Col> {
        self.columns
            .required
            .iter()
            .filter_map(|k| Col::from_key(k))
            .collect()
    }

    pub fn tz(&self) -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(self.ticket.utc_offset_hours * 3600)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
    }

    /// 今日の日付 (YYYY-MM-DD)
    pub fn today(&self) -> String {
        chrono::Utc::now()
            .with_timezone(&self.tz())
            .format("%Y-%m-%d")
            .to_string()
    }

    pub fn now(&self) -> String {
        chrono::Utc::now()
            .with_timezone(&self.tz())
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    }
}

/// "projects/<プロジェクトID>/subscriptions/<名前>" 形式か
fn is_subscription_path(s: &str) -> bool {
    matches!(
        s.split('/').collect::<Vec<_>>()[..],
        ["projects", project, "subscriptions", name] if !project.is_empty() && !name.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_path() {
        assert!(is_subscription_path("projects/ttm-db/subscriptions/bot"));
        assert!(!is_subscription_path("ttm-db/subscriptions/bot"));
        assert!(!is_subscription_path("projects/ttm-db/topics/bot"));
        assert!(!is_subscription_path("projects//subscriptions/bot"));
        assert!(!is_subscription_path("projects/ttm-db/subscriptions/bot/x"));
    }

    #[test]
    fn sample_config_loads() {
        let cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        assert_eq!(cfg.header(Col::TicketId), "ticket_id");
        assert_eq!(cfg.required_cols().len(), 13);
        assert_eq!(cfg.ticket.id_prefix, "T-");
        assert_eq!(cfg.sheets.header_row, 1);
    }

    #[test]
    fn env_overrides() {
        let mut cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let vars: HashMap<&str, &str> = [
            ("TTM_TICKETS_SHEET", "チケット"),
            ("TTM_COL_TITLE", "件名"),
            ("TTM_MEMBERS", "123=田中, alice=佐藤"),
            ("TTM_HEADER_ROW", "3"),
        ]
        .into();
        cfg.apply_env(
            |n| vars.get(n).map(|s| s.to_string()),
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        )
        .unwrap();
        assert_eq!(cfg.sheets.tickets_sheet, "チケット");
        assert_eq!(cfg.header(Col::Title), "件名");
        assert_eq!(cfg.sheets.header_row, 3);
        assert_eq!(cfg.member_name(123, "bob"), Some("田中"));
        assert_eq!(cfg.member_name(999, "alice"), Some("佐藤"));
        assert_eq!(cfg.member_name(999, "bob"), None);
        assert_eq!(cfg.member_id_for_name("田中"), Some(123));
        assert_eq!(cfg.member_id_for_name("佐藤"), None);

        let bad: HashMap<&str, &str> = [("TTM_COL_NOPE", "x")].into();
        assert!(
            cfg.apply_env(
                |n| bad.get(n).map(|s| s.to_string()),
                bad.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            )
            .is_err()
        );

        // 廃止した列は理由付きでエラーにする
        let removed: HashMap<&str, &str> = [("TTM_COL_REPORTER_ID", "発行者ID")].into();
        let err = cfg
            .apply_env(
                |n| removed.get(n).map(|s| s.to_string()),
                removed.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            )
            .unwrap_err();
        assert!(err.to_string().contains("廃止"));
    }
}
