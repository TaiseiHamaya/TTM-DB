/**
 * TTM-DB: tickets / masters シートが人に編集されたら、Google Cloud Pub/Sub のトピックへ合図を送る。
 * Bot はサブスクリプションから合図を受け取り、Sheets API でシートを読み直して Discord に反映する。
 * 合図にシートの内容は載せない。
 *
 * 編集トリガー (onEdit) は人の編集でのみ動き、Bot の Sheets API 書き込みでは動かない。
 * 変更トリガー (onChange) は API 書き込みでも動き、チケット発行のたびに送ってしまうので使わない。
 * Discord の Webhook には送らない (Apps Script の送信元 IP は共有のため、Discord 手前の
 * Cloudflare にまとめてレート制限されることがある)。
 *
 * セットアップ:
 *   1. 拡張機能 > Apps Script にこのファイルを貼る
 *   2. プロジェクトの設定で「appsscript.json マニフェスト ファイルをエディタで表示する」をオンにし、
 *      appsscript.json を gas/appsscript.json の内容にする
 *   3. プロジェクトの設定 > スクリプト プロパティに PUBSUB_TOPIC
 *      (projects/<プロジェクトID>/topics/<トピック名>) を追加する
 *   4. 下の TICKETS_SHEET / MASTERS_SHEET を Bot の config.toml と同じシート名にする
 *   5. setupTrigger を1回だけ実行して権限を承認する (編集トリガーが作られ、旧版のトリガーは消える)
 */

const TICKETS_SHEET = 'tickets';
const MASTERS_SHEET = 'masters';
const MAX_RETRY = 3;
// 旧版が作っていたトリガーのハンドラ名 (setupTrigger で削除する)
const OLD_HANDLERS = ['onSheetChange'];

/** 編集トリガーを作成する (既存のものと旧版のトリガーは作り直す) */
function setupTrigger() {
  ScriptApp.getProjectTriggers()
    .filter((t) => {
      const h = t.getHandlerFunction();
      return h === 'onSheetEdit' || OLD_HANDLERS.indexOf(h) >= 0;
    })
    .forEach((t) => ScriptApp.deleteTrigger(t));
  ScriptApp.newTrigger('onSheetEdit')
    .forSpreadsheet(SpreadsheetApp.getActive())
    .onEdit()
    .create();
}

/** インストール型の編集トリガー (人の編集でのみ動き、Bot の API 書き込みでは動かない) */
function onSheetEdit(e) {
  // 対象外のシートの編集は送らない
  const name = e && e.range ? e.range.getSheet().getName() : '';
  if (name !== TICKETS_SHEET && name !== MASTERS_SHEET) return;
  publish(name);
}

/** 動作確認用: エディタから実行するとトピックに合図を1件送る */
function testPublish() {
  publish(TICKETS_SHEET);
}

/** Pub/Sub のトピックに「シートが編集された」合図を送る */
function publish(sheet) {
  const topic = PropertiesService.getScriptProperties().getProperty('PUBSUB_TOPIC');
  if (!topic) throw new Error('スクリプト プロパティ PUBSUB_TOPIC が未設定です');
  const m = /^projects\/([^/]+)\/topics\/[^/]+$/.exec(topic);
  if (!m) throw new Error(`PUBSUB_TOPIC「${topic}」は projects/<プロジェクトID>/topics/<トピック名> 形式で指定してください`);
  const data = Utilities.base64Encode(JSON.stringify({ sheet: sheet, edited_at: Date.now() }));
  for (let i = 0; i < MAX_RETRY; i++) {
    const res = UrlFetchApp.fetch(`https://pubsub.googleapis.com/v1/${topic}:publish`, {
      method: 'post',
      contentType: 'application/json',
      headers: {
        Authorization: `Bearer ${ScriptApp.getOAuthToken()}`,
        // API の利用をトピックのプロジェクトに付ける (Apps Script 既定のプロジェクトでは Pub/Sub API が無効のため)
        'x-goog-user-project': m[1],
      },
      payload: JSON.stringify({ messages: [{ data: data }] }),
      muteHttpExceptions: true,
    });
    const code = res.getResponseCode();
    if (code >= 200 && code < 300) return;
    // 一時的なエラーだけ再送する
    if (code !== 429 && code < 500) {
      throw new Error(`Pub/Sub エラー ${code}: ${res.getContentText()}`);
    }
    Utilities.sleep(1000 * (i + 1));
  }
  throw new Error('Pub/Sub への送信が一時的なエラーで失敗し続けました');
}
