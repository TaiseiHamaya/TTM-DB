//! スプシ編集の通知を Google Cloud Pub/Sub で受け取り、Sheets を読み直して Discord に反映する。
//!
//! スプシの GAS (gas/sync.gs) が人の編集のたびにトピックへ合図を送り、Bot はサブスクリプションを
//! ロングポーリング (pull) して受け取る。Bot から接続しに行くだけなので外部に公開する受け口は要らない。
//! 合図にシートの内容は載せず、受け取ったら Bot が Sheets API で読む。続けて編集されたときは少し待って
//! まとめ、1回だけ読む

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::mpsc;

use crate::app::{App, Data};
use crate::ops;

const SCOPES: &[&str] = &["https://www.googleapis.com/auth/pubsub"];
const BASE: &str = "https://pubsub.googleapis.com/v1";
/// 続けて編集されたときに、まとめて1回だけ読むための待ち時間
const DEBOUNCE: Duration = Duration::from_millis(1500);
/// pull の応答待ちの上限。Pub/Sub はメッセージが無いとしばらく応答を保留する
const PULL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// 受信の失敗がこの回数続いたら警告チャンネルに知らせる (一時的な失敗では知らせない)
const ALERT_AFTER_FAILURES: u32 = 3;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullResponse {
    #[serde(default)]
    received_messages: Vec<ReceivedMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceivedMessage {
    ack_id: String,
}

/// 受信と反映のタスクを起動する
pub fn spawn(app: Data, subscription: String) {
    // 容量1: 反映待ちの合図が既にあれば、新しい合図はそれにまとめる
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(receive_loop(app.clone(), subscription, tx));
    tokio::spawn(sync_loop(app, rx));
}

async fn receive_loop(app: Data, subscription: String, tx: mpsc::Sender<()>) {
    tracing::info!(subscription = %subscription, "スプシ編集の通知の受信を開始");
    let mut backoff = Duration::from_secs(1);
    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        match pull(&app, &subscription).await {
            Ok(n) => {
                if failures >= ALERT_AFTER_FAILURES {
                    ops::alert(
                        &app,
                        "✅ TTM-DB: スプシ編集の通知の受信が回復しました。".to_owned(),
                    )
                    .await;
                }
                failures = 0;
                backoff = Duration::from_secs(1);
                if n > 0 {
                    let _ = tx.try_send(());
                } else if started.elapsed() < Duration::from_secs(1) {
                    // 保留せず空で返ってきた場合に問い合わせを繰り返し過ぎない
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(error = %e, failures, "スプシ編集の通知の受信に失敗");
                if failures == ALERT_AFTER_FAILURES {
                    let msg = format!(
                        "⚠ TTM-DB: スプシ編集の通知を受信できません。回復するまで `/sync` で反映してください。\n{e:#}"
                    );
                    ops::alert(&app, msg).await;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn sync_loop(app: Data, mut rx: mpsc::Receiver<()>) {
    let mut failing = false;
    while rx.recv().await.is_some() {
        tokio::time::sleep(DEBOUNCE).await;
        // 待つ間に届いた合図もこの1回で反映する
        while rx.try_recv().is_ok() {}
        match ops::sync_from_sheets(&app).await {
            Ok(n) => {
                tracing::info!(count = n, "スプシの編集を反映");
                if failing {
                    failing = false;
                    ops::alert(
                        &app,
                        "✅ TTM-DB: スプシの編集の反映が回復しました。".to_owned(),
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "スプシの編集の反映に失敗");
                // 失敗が続いても警告は最初の1回だけ
                if !failing {
                    failing = true;
                    let msg = format!("⚠ TTM-DB: スプシの編集を反映できませんでした: {e:#}");
                    ops::alert(&app, msg).await;
                }
            }
        }
    }
}

/// サブスクリプションから受け取って確認応答 (ack) し、受け取った件数を返す
async fn pull(app: &App, subscription: &str) -> Result<usize> {
    let token = app.sheets.google_token(SCOPES).await?;
    let res = app
        .web
        .post(format!("{BASE}/{subscription}:pull"))
        .bearer_auth(&token)
        .timeout(PULL_TIMEOUT)
        .json(&json!({ "maxMessages": 100 }))
        .send()
        .await;
    let res = match res {
        Err(e) if e.is_timeout() => return Ok(0),
        res => res.context("Pub/Sub に接続できません")?,
    };
    let pulled: PullResponse = parse(res).await?;
    let n = pulled.received_messages.len();
    if n == 0 {
        return Ok(0);
    }
    let ack_ids: Vec<&str> = pulled
        .received_messages
        .iter()
        .map(|m| m.ack_id.as_str())
        .collect();
    let acked = async {
        let res = app
            .web
            .post(format!("{BASE}/{subscription}:acknowledge"))
            .bearer_auth(&token)
            .json(&json!({ "ackIds": ack_ids }))
            .send()
            .await?;
        parse::<serde_json::Value>(res).await
    }
    .await;
    // ack できなくても受け取った合図は反映する (再配信されても差分が無ければ何もしない)
    if let Err(e) = acked {
        tracing::warn!(error = %e, "Pub/Sub の確認応答に失敗");
    }
    Ok(n)
}

async fn parse<T: DeserializeOwned>(res: reqwest::Response) -> Result<T> {
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        bail!("Pub/Sub API エラー {status}: {text}");
    }
    serde_json::from_str(&text).context("Pub/Sub API の応答を解釈できません")
}
