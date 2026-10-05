//! Bot 全体で共有する状態。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use twilight_http::Client;
use twilight_http::client::InteractionClient;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, GuildMarker, TagMarker};

use crate::config::{Config, Env};
use crate::create::Draft;
use crate::masters::Masters;
use crate::sheets::Sheets;
use crate::store::{Store, Table};

pub type Data = Arc<App>;

pub struct App {
    pub env: Env,
    pub cfg: Config,
    pub sheets: Sheets,
    /// Discord REST API
    pub http: Client,
    pub application_id: Id<ApplicationMarker>,
    /// 添付ファイルのダウンロード用
    pub web: reqwest::Client,
    pub masters: RwLock<Masters>,
    /// Forum タグ名 -> タグ ID
    pub tags: RwLock<HashMap<String, Id<TagMarker>>>,
    /// スキーマ破壊を検出して同期停止中なら、その理由
    pub halted: RwLock<Option<String>>,
    /// Bot による tickets への書き込みを直列化する (発番の競合・スプシからの同期との競合回避)
    pub write_lock: Mutex<()>,
    pub sync_state: Mutex<SyncState>,
    /// 発行フォームの入力途中の内容 (キーはフォームごとの ID)
    pub drafts: Mutex<HashMap<String, Draft>>,
    /// オートコンプリート用の短命キャッシュ
    table_cache: Mutex<Option<(Instant, Arc<Table>)>>,
}

impl App {
    pub fn new(
        env: Env,
        cfg: Config,
        sheets: Sheets,
        masters: Masters,
        http: Client,
        application_id: Id<ApplicationMarker>,
    ) -> Self {
        let sync_state = SyncState::load(&cfg.sync.state_file);
        Self {
            env,
            cfg,
            sheets,
            http,
            application_id,
            web: reqwest::Client::new(),
            masters: RwLock::new(masters),
            tags: RwLock::new(HashMap::new()),
            halted: RwLock::new(None),
            write_lock: Mutex::new(()),
            sync_state: Mutex::new(sync_state),
            drafts: Mutex::new(HashMap::new()),
            table_cache: Mutex::new(None),
        }
    }

    pub fn store(&self) -> Store<'_> {
        Store {
            sheets: &self.sheets,
            cfg: &self.cfg,
        }
    }

    pub fn interaction(&self) -> InteractionClient<'_> {
        self.http.interaction(self.application_id)
    }

    pub fn guild_id(&self) -> Id<GuildMarker> {
        Id::new(self.env.guild_id)
    }

    pub fn forum_id(&self) -> Id<ChannelMarker> {
        Id::new(self.env.forum_channel_id)
    }

    /// 同期停止中なら操作を拒否する
    pub async fn ensure_running(&self) -> Result<()> {
        if let Some(reason) = self.halted.read().await.as_ref() {
            bail!("スプシの構成に問題があるため同期を停止中です: {reason}");
        }
        Ok(())
    }

    /// 最新の tickets を読む (正本の参照。キャッシュも更新する)
    pub async fn load_table(&self) -> Result<Table> {
        let table = self.store().load().await?;
        self.cache_table(&table).await;
        Ok(table)
    }

    /// GAS から届いた内容などでキャッシュを差し替える
    pub async fn cache_table(&self, table: &Table) {
        *self.table_cache.lock().await = Some((Instant::now(), Arc::new(table.clone())));
    }

    /// オートコンプリート等、多少古くてよい用途向け (30秒キャッシュ)
    pub async fn cached_table(&self) -> Result<Arc<Table>> {
        if let Some((at, t)) = self.table_cache.lock().await.as_ref()
            && at.elapsed() < Duration::from_secs(30)
        {
            return Ok(t.clone());
        }
        Ok(Arc::new(self.load_table().await?))
    }

    /// Discord メンバーのスプシ上の表示名。
    /// 優先順: 設定 (config.toml [members] / TTM_MEMBERS) > masters の assignee > Discord 表示名
    pub async fn sheet_name_of(&self, user_id: u64, username: &str, display_name: &str) -> String {
        if let Some(name) = self.cfg.member_name(user_id, username) {
            return name.to_owned();
        }
        if let Some(a) = self.masters.read().await.assignee_by_id(user_id) {
            return a.name.clone();
        }
        display_name.to_owned()
    }

    pub async fn reload_masters(&self) -> Result<()> {
        let m = Masters::load(&self.sheets, &self.cfg).await?;
        *self.masters.write().await = m;
        Ok(())
    }
}

/// インタラクションに付随するメンバー権限から管理者相当かを判定する
pub fn is_admin(perms: Option<Permissions>) -> bool {
    perms.is_some_and(|p| {
        p.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_THREADS | Permissions::MANAGE_GUILD)
    })
}

/// サーバーでの表示名 (ニックネーム > 表示名 > ユーザー名)
pub fn display_name<'a>(nick: Option<&'a str>, user: &'a twilight_model::user::User) -> &'a str {
    nick.or(user.global_name.as_deref()).unwrap_or(&user.name)
}

/// 差分検出用に、最後に Discord へ反映した内容ハッシュを保存する
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub hashes: HashMap<String, u64>,
    /// 同期失敗リアクションを付けた ticket_id
    pub failed: HashSet<String>,
    /// 状態ファイルが新規作成された (初回の同期では全件反映せず現状を記録するだけ)
    #[serde(skip)]
    pub fresh: bool,
    /// ticket_id -> 最後に Discord へ反映した時刻 (UNIX ミリ秒)。
    /// これより前に GAS が読んだ内容は古いので適用しない
    #[serde(skip)]
    pub pushed_at: HashMap<String, i64>,
    /// 最後に適用した GAS 送信内容の読み取り時刻 (UNIX ミリ秒)。順序が入れ替わって届いた古い内容を捨てる
    #[serde(skip)]
    pub last_snapshot_at: i64,
    #[serde(skip)]
    path: String,
}

impl SyncState {
    fn load(path: &str) -> Self {
        let mut s = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<SyncState>(&t).ok())
            .unwrap_or_else(|| SyncState {
                fresh: true,
                ..Default::default()
            });
        s.path = path.to_owned();
        s
    }

    pub fn save(&self) {
        if let Some(dir) = Path::new(&self.path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&self.path, text) {
                    tracing::warn!(error = %e, path = %self.path, "同期状態の保存に失敗");
                }
            }
            Err(e) => tracing::warn!(error = %e, "同期状態のシリアライズに失敗"),
        }
    }
}
