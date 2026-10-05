/**
 * TTM-DB: スプシが編集されたら tickets / masters シートの内容を Discord Webhook へ送る。
 * Bot は Webhook の投稿 (添付 sync.json) を受け取り、差分を Forum Post に反映する。
 *
 * セットアップ:
 *   1. 拡張機能 > Apps Script にこのファイルを貼る
 *   2. プロジェクトの設定 > スクリプト プロパティに DISCORD_WEBHOOK_URL を追加する
 *   3. 下の TICKETS_SHEET / MASTERS_SHEET を Bot の config.toml と同じシート名にする
 *   4. setupTrigger を1回だけ実行して権限を承認する (変更トリガーが作られる)
 *
 * Bot (API) による書き込みではトリガーは動かないので、送信がループすることはない。
 */

const TICKETS_SHEET = 'tickets';
const MASTERS_SHEET = 'masters';
// 書式変更だけのときは送らない
const SKIP_CHANGE_TYPES = ['FORMAT'];
const MAX_RETRY = 5;

/** 変更トリガーを作成する (既存のものは作り直す) */
function setupTrigger() {
  ScriptApp.getProjectTriggers()
    .filter((t) => t.getHandlerFunction() === 'onSheetChange')
    .forEach((t) => ScriptApp.deleteTrigger(t));
  ScriptApp.newTrigger('onSheetChange')
    .forSpreadsheet(SpreadsheetApp.getActive())
    .onChange()
    .create();
}

/** 手動で送るためのメニュー */
function onOpen() {
  SpreadsheetApp.getUi()
    .createMenu('TTM-DB')
    .addItem('Discord へ同期', 'pushToDiscord')
    .addToUi();
}

/** インストール型の変更トリガー */
function onSheetChange(e) {
  if (e && SKIP_CHANGE_TYPES.indexOf(e.changeType) >= 0) return;
  if (e && e.changeType === 'EDIT') {
    // 対象外のシートの編集は送らない
    const name = SpreadsheetApp.getActiveSheet().getName();
    if (name !== TICKETS_SHEET && name !== MASTERS_SHEET) return;
  }
  pushToDiscord();
}

/** tickets / masters の表示値を読んで Webhook に送る */
function pushToDiscord() {
  const url = PropertiesService.getScriptProperties().getProperty('DISCORD_WEBHOOK_URL');
  if (!url) throw new Error('スクリプト プロパティ DISCORD_WEBHOOK_URL が未設定です');

  // 連続編集で送信が前後しないよう直列化する
  const lock = LockService.getScriptLock();
  lock.waitLock(30 * 1000);
  try {
    const ss = SpreadsheetApp.getActive();
    const snapshot = {
      read_at: Date.now(),
      tickets: displayValues(ss.getSheetByName(TICKETS_SHEET), null),
      masters: displayValues(ss.getSheetByName(MASTERS_SHEET), 3),
    };
    const rows = snapshot.tickets ? Math.max(snapshot.tickets.length - 1, 0) : 0;
    const payload = {
      payload_json: JSON.stringify({
        content: `TTM-DB sync: tickets ${rows}行`,
        allowed_mentions: { parse: [] },
      }),
      'files[0]': Utilities.newBlob(JSON.stringify(snapshot), 'application/json', 'sync.json'),
    };
    send(url, payload);
  } finally {
    lock.releaseLock();
  }
}

/** シートの表示値 (Bot の Sheets API 読み込みと同じ FORMATTED_VALUE 相当)。シートが無ければ null */
function displayValues(sheet, cols) {
  if (!sheet) return null;
  const lastRow = sheet.getLastRow();
  const lastCol = cols ? Math.min(cols, sheet.getMaxColumns()) : sheet.getLastColumn();
  if (lastRow === 0 || lastCol === 0) return [];
  return sheet.getRange(1, 1, lastRow, lastCol).getDisplayValues();
}

/** Webhook に送る。レート制限 (429) なら待って再送する */
function send(url, payload) {
  for (let i = 0; i < MAX_RETRY; i++) {
    const res = UrlFetchApp.fetch(url + '?wait=true', {
      method: 'post',
      payload: payload,
      muteHttpExceptions: true,
    });
    const code = res.getResponseCode();
    if (code >= 200 && code < 300) return;
    if (code === 429) {
      const body = JSON.parse(res.getContentText() || '{}');
      Utilities.sleep(Math.ceil((body.retry_after || 1) * 1000));
      continue;
    }
    throw new Error(`Discord Webhook エラー ${code}: ${res.getContentText()}`);
  }
  throw new Error('Discord Webhook のレート制限が解除されませんでした');
}
