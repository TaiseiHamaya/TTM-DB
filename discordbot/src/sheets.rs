//! Google Sheets API v4 の薄いクライアント (Service Account 認証)。

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use yup_oauth2::authenticator::DefaultAuthenticator;

const SCOPES: &[&str] = &["https://www.googleapis.com/auth/spreadsheets"];
const BASE: &str = "https://sheets.googleapis.com/v4/spreadsheets";

pub struct Sheets {
    http: reqwest::Client,
    auth: DefaultAuthenticator,
    spreadsheet_id: String,
}

#[derive(Deserialize)]
struct ValueRange {
    #[serde(default)]
    values: Vec<Vec<Value>>,
}

impl Sheets {
    /// `sa_json` は JSON ファイルのパス、または JSON 文字列そのもの
    pub async fn new(sa_json: &str, spreadsheet_id: String) -> Result<Self> {
        let key = if sa_json.trim_start().starts_with('{') {
            yup_oauth2::parse_service_account_key(sa_json)
        } else {
            yup_oauth2::read_service_account_key(sa_json).await
        }
        .context("Service Account JSON を読めません")?;
        let auth = yup_oauth2::ServiceAccountAuthenticator::builder(key)
            .build()
            .await
            .context("Service Account 認証の初期化に失敗しました")?;
        Ok(Self {
            http: reqwest::Client::new(),
            auth,
            spreadsheet_id,
        })
    }

    pub fn spreadsheet_url(&self) -> String {
        format!(
            "https://docs.google.com/spreadsheets/d/{}/edit",
            self.spreadsheet_id
        )
    }

    async fn token(&self) -> Result<String> {
        self.google_token(SCOPES).await
    }

    /// 同じ Service Account で、指定スコープの Google API アクセストークンを取得する (Pub/Sub など)
    pub async fn google_token(&self, scopes: &[&str]) -> Result<String> {
        let tok = self
            .auth
            .token(scopes)
            .await
            .context("アクセストークン取得失敗")?;
        tok.token()
            .map(str::to_owned)
            .context("アクセストークンが空です")
    }

    fn url(&self, suffix: &[&str]) -> reqwest::Url {
        let mut url = reqwest::Url::parse(BASE).unwrap();
        {
            let mut seg = url.path_segments_mut().unwrap();
            seg.push(&self.spreadsheet_id);
            for s in suffix {
                seg.push(s);
            }
        }
        url
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let res = req.bearer_auth(self.token().await?).send().await?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("Sheets API エラー {status}: {body}");
        }
        Ok(body)
    }

    /// 範囲の値を文字列の2次元配列で取得する (表示値)
    pub async fn get(&self, range: &str) -> Result<Vec<Vec<String>>> {
        let url = self.url(&["values", range]);
        let body = self
            .send(
                self.http
                    .get(url)
                    .query(&[("valueRenderOption", "FORMATTED_VALUE")]),
            )
            .await?;
        let vr: ValueRange = serde_json::from_value(body)?;
        Ok(vr
            .values
            .into_iter()
            .map(|row| row.into_iter().map(value_to_string).collect())
            .collect())
    }

    /// シートの行数 (値の有無に関わらずグリッドの行数)
    pub async fn row_count(&self, sheet: &str) -> Result<usize> {
        let body = self
            .send(
                self.http
                    .get(self.url(&[]))
                    .query(&[("fields", "sheets.properties(title,gridProperties.rowCount)")]),
            )
            .await?;
        body["sheets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| &s["properties"])
            .find(|p| p["title"] == sheet)
            .and_then(|p| p["gridProperties"]["rowCount"].as_u64())
            .map(|n| n as usize)
            .with_context(|| format!("シート {sheet} の行数を取得できません"))
    }

    /// `start_row` 行目から始まる表の末尾に1行追加し、追加された行番号 (1始まり) を返す。
    /// 表の上にタイトル行などがあっても、その下に誤って追加しないよう開始行を指定する。
    /// 値は文字列のまま書く (RAW)
    pub async fn append_row(
        &self,
        sheet: &str,
        start_row: usize,
        row: Vec<String>,
    ) -> Result<usize> {
        let range = format!("{}!A{start_row}", quote_sheet(sheet));
        let url = self.url(&["values", &format!("{range}:append")]);
        let body = self
            .send(
                self.http
                    .post(url)
                    .query(&[
                        ("valueInputOption", Input::Raw.as_str()),
                        ("insertDataOption", "INSERT_ROWS"),
                    ])
                    .json(&json!({ "values": [row] })),
            )
            .await?;
        let updated = body["updates"]["updatedRange"]
            .as_str()
            .context("append の応答に updatedRange がありません")?;
        parse_row_of_range(updated).context("updatedRange の行番号を解釈できません")
    }

    /// 複数セルを個別に更新する。(A1範囲, 値) の組。未指定セルには触れない
    pub async fn update_cells(&self, cells: Vec<(String, String)>, input: Input) -> Result<()> {
        if cells.is_empty() {
            return Ok(());
        }
        let data: Vec<Value> = cells
            .into_iter()
            .map(|(range, v)| json!({ "range": range, "values": [[v]] }))
            .collect();
        let url = self.url(&["values:batchUpdate"]);
        self.send(
            self.http
                .post(url)
                .json(&json!({ "valueInputOption": input.as_str(), "data": data })),
        )
        .await?;
        Ok(())
    }
}

fn value_to_string(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 書き込む値の解釈方法
#[derive(Clone, Copy)]
pub enum Input {
    /// 文字列のまま書く。数式・数値・日付として解釈されない
    Raw,
    /// 手入力と同様に解釈させる (日付を日付として書く)
    UserEntered,
}

impl Input {
    fn as_str(self) -> &'static str {
        match self {
            Input::Raw => "RAW",
            Input::UserEntered => "USER_ENTERED",
        }
    }
}

/// シート名を A1 表記用にクォートする
pub fn quote_sheet(sheet: &str) -> String {
    format!("'{}'", sheet.replace('\'', "''"))
}

/// 0始まりの列インデックスを A, B, ..., Z, AA ... に変換
pub fn col_letter(mut idx: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (idx % 26) as u8);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

/// "'tickets'!A5:P5" -> 5
fn parse_row_of_range(range: &str) -> Option<usize> {
    let cells = range.rsplit('!').next()?;
    let first = cells.split(':').next()?;
    first
        .trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '$')
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(27), "AB");
        assert_eq!(col_letter(701), "ZZ");
        assert_eq!(col_letter(702), "AAA");
    }

    #[test]
    fn row_of_range() {
        assert_eq!(parse_row_of_range("'tickets'!A5:P5"), Some(5));
        assert_eq!(parse_row_of_range("tickets!A120"), Some(120));
    }
}
