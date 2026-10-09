//! tickets シートの読み書き。
//! 列はヘッダ名で参照し、未知列には一切触れない (行更新はセル単位の差分のみ)。

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::{Col, Config};
use crate::sheets::{Sheets, col_letter, quote_sheet, text};

#[derive(Debug, Clone)]
pub struct Ticket {
    /// シート上の行番号 (1始まり、ヘッダが1行目)
    pub row: usize,
    cells: HashMap<Col, String>,
}

impl Ticket {
    pub fn get(&self, col: Col) -> &str {
        self.cells.get(&col).map(|s| s.trim()).unwrap_or("")
    }

    pub fn id(&self) -> &str {
        self.get(Col::TicketId)
    }

    pub fn set(&mut self, col: Col, v: impl Into<String>) {
        self.cells.insert(col, v.into());
    }

    pub fn post_id(&self) -> Option<u64> {
        self.get(Col::DiscordPostId).parse().ok()
    }

    pub fn image_urls(&self) -> Vec<&str> {
        self.get(Col::ImageUrls)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Discord 表示に関わる内容のハッシュ (スプシからの同期の差分検出用, FNV-1a)
    pub fn content_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for col in Col::ALL {
            for b in self.get(col).bytes().chain(std::iter::once(0x1f)) {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        h
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    pub headers: Vec<String>,
    col_idx: HashMap<Col, usize>,
    pub tickets: Vec<Ticket>,
}

impl Table {
    /// シートの値 (1行目から) を組み立てる。ヘッダは sheets.header_row 行目で、それより下をチケットとして読む。
    /// 必須列が欠けていれば SchemaError
    pub fn from_rows(rows: &[Vec<String>], cfg: &Config) -> Result<Table> {
        let header_idx = cfg.sheets.header_row - 1;
        let headers: Vec<String> = rows
            .get(header_idx)
            .map(|r| r.iter().map(|h| h.trim().to_owned()).collect())
            .unwrap_or_default();
        let mut col_idx = HashMap::new();
        for col in Col::ALL {
            if let Some(i) = headers.iter().position(|h| h == cfg.header(col)) {
                col_idx.insert(col, i);
            }
        }
        let missing: Vec<String> = cfg
            .required_cols()
            .into_iter()
            .filter(|c| !col_idx.contains_key(c))
            .map(|c| cfg.header(c).to_owned())
            .collect();
        if !missing.is_empty() {
            return Err(SchemaError(missing).into());
        }
        // 既知列が全て空の行は空行 (未知列のチェックボックス等の初期値は無視する)
        let tickets = rows
            .iter()
            .enumerate()
            .skip(header_idx + 1)
            .filter(|(_, r)| {
                col_idx
                    .values()
                    .any(|&i| r.get(i).is_some_and(|c| !c.trim().is_empty()))
            })
            .map(|(i, r)| {
                let mut cells: HashMap<Col, String> = col_idx
                    .iter()
                    .map(|(&c, &idx)| (c, r.get(idx).cloned().unwrap_or_default()))
                    .collect();
                // 日付セルは表示形式に依らず YYYY-MM-DD に揃える。解釈できなければそのまま (/sync check で検出)
                if let Some(due) = cells.get_mut(&Col::DueDate)
                    && let Some(d) = crate::ops::normalize_date(due)
                {
                    *due = d;
                }
                Ticket { row: i + 1, cells }
            })
            .collect();
        Ok(Table {
            headers,
            col_idx,
            tickets,
        })
    }

    pub fn find(&self, ticket_id: &str) -> Option<&Ticket> {
        let id = ticket_id.trim();
        self.tickets
            .iter()
            .find(|t| t.id().eq_ignore_ascii_case(id))
    }

    pub fn find_by_post(&self, post_id: u64) -> Option<&Ticket> {
        self.tickets.iter().find(|t| t.post_id() == Some(post_id))
    }

    pub fn children_of<'a>(&'a self, ticket_id: &'a str) -> impl Iterator<Item = &'a Ticket> + 'a {
        self.tickets
            .iter()
            .filter(move |t| t.get(Col::ParentId).eq_ignore_ascii_case(ticket_id))
    }

    /// 次の連番 ID (既存の最大番号 + 1)
    pub fn next_id(&self, cfg: &Config) -> String {
        let prefix = &cfg.ticket.id_prefix;
        let max = self
            .tickets
            .iter()
            .filter_map(|t| t.id().strip_prefix(prefix.as_str()))
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().ok()
            })
            .max()
            .unwrap_or(0);
        format!("{prefix}{:0width$}", max + 1, width = cfg.ticket.id_digits)
    }
}

/// 必須列の欠落 (ヘッダ名で検出)
#[derive(Debug)]
pub struct SchemaError(pub Vec<String>);

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tickets シートの必須列が見つかりません: {}",
            self.0.join(", ")
        )
    }
}

impl std::error::Error for SchemaError {}

pub struct Store<'a> {
    pub sheets: &'a Sheets,
    pub cfg: &'a Config,
}

impl Store<'_> {
    fn sheet(&self) -> String {
        quote_sheet(&self.cfg.sheets.tickets_sheet)
    }

    /// 全行を読み込む。必須列が欠けていれば SchemaError
    pub async fn load(&self) -> Result<Table> {
        let rows = self.sheets.get(&self.sheet()).await?;
        Table::from_rows(&rows, self.cfg)
    }

    /// 最終チケットの次の行に書き込み、行番号を返す。
    /// 書式・入力規則を設定済みの空行を使うため行は挿入せず、指定列のセルだけを書く (未知列には触れない)。
    /// 用意された行を使い切っているときだけ末尾に行を追加する
    pub async fn append(&self, table: &Table, values: &[(Col, String)]) -> Result<usize> {
        let row = table
            .tickets
            .last()
            .map_or(self.cfg.sheets.header_row, |t| t.row)
            + 1;
        let row_count = self.sheets.row_count(&self.cfg.sheets.tickets_sheet).await?;
        if row > row_count {
            tracing::warn!(
                sheets_row = row,
                "書式を設定済みの行を使い切ったため、行を挿入して追記します"
            );
            let mut cells = vec![String::new(); table.headers.len()];
            for (col, v) in values {
                if let Some(&i) = table.col_idx.get(col) {
                    cells[i] = cell_input(*col, v);
                }
            }
            return self
                .sheets
                .append_row(&self.cfg.sheets.tickets_sheet, self.cfg.sheets.header_row, cells)
                .await;
        }
        let values: Vec<_> = values.iter().filter(|(_, v)| !v.is_empty()).cloned().collect();
        self.sheets.update_cells(self.cells(table, row, &values)).await?;
        Ok(row)
    }

    /// (列, 値) を指定行の (A1範囲, 入力値) に変換する。シートに無い列は捨てる
    fn cells(&self, table: &Table, row: usize, values: &[(Col, String)]) -> Vec<(String, String)> {
        values
            .iter()
            .filter_map(|(col, v)| {
                let idx = table.col_idx.get(col)?;
                Some((
                    format!("{}!{}{row}", self.sheet(), col_letter(*idx)),
                    cell_input(*col, v),
                ))
            })
            .collect()
    }

    /// 指定行の指定セルだけを更新する。直前に ticket_id を再確認し、行ずれしていたら中止する
    pub async fn update(
        &self,
        table: &Table,
        row: usize,
        expect_id: &str,
        changes: &[(Col, String)],
    ) -> Result<()> {
        let id_idx = table.col_idx[&Col::TicketId];
        let id_range = format!("{}!{}{row}", self.sheet(), col_letter(id_idx));
        let current = self.sheets.get(&id_range).await?;
        let current_id = current
            .first()
            .and_then(|r| r.first())
            .map(|s| s.trim())
            .unwrap_or("");
        if current_id != expect_id {
            bail!(
                "シートの {row} 行目が {expect_id} ではなくなっています (並べ替え等)。/sync で再読込してください"
            );
        }
        self.sheets
            .update_cells(self.cells(table, row, changes))
            .await
    }
}

/// USER_ENTERED で書く値。期限は日付として解釈させ (日付の入力規則・表示形式を効かせる)、
/// それ以外は文字列に固定する
fn cell_input(col: Col, v: &str) -> String {
    match col {
        Col::DueDate => v.to_owned(),
        _ => text(v),
    }
}
