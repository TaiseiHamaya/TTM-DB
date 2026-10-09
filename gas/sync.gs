/**
 * TTM-DB: tickets / masters シートの内容を Discord Webhook へ送る。
 * Bot は Webhook のメッセージ (添付 sync.json) を受け取り、差分を Forum Post に反映する。
 *
 * 人がスプシを編集すると (編集トリガー)、同期チャンネルの固定メッセージ1件の sync.json を
 * 差し替える。新しい投稿はしない。Bot はメッセージの更新イベントで反映する。
 * 変更トリガー (onChange) は Bot の Sheets API 書き込みでも動き、チケット発行のたびに送信されて
 * しまうので使わない。編集トリガー (onEdit) は API 書き込みでは動かない。
 * Discord で /sync を実行したときの sync.json は Bot が自分で投稿するので、ここでは扱わない。
 *
 * セットアップ:
 *   1. 拡張機能 > Apps Script にこのファイルを貼る
 *   2. プロジェクトの設定 > スクリプト プロパティに DISCORD_WEBHOOK_URL (同期チャンネルの Webhook URL) を追加する
 *      固定メッセージの ID は初回送信時に SYNC_MESSAGE_ID として自動で保存される
 *   3. 下の TICKETS_SHEET / MASTERS_SHEET を Bot の config.toml と同じシート名にする
 *   4. setupTrigger を1回だけ実行して権限を承認する (編集トリガーが作られ、旧版の変更トリガーは消える)
 */

const TICKETS_SHEET = 'tickets';
const MASTERS_SHEET = 'masters';
const MAX_RETRY = 5;
// 旧版が作っていた変更トリガーのハンドラ名 (setupTrigger で削除する)
const OLD_HANDLERS = ['onSheetChange'];
// 編集で差し替える固定メッセージの ID を保存するスクリプト プロパティ
const MESSAGE_ID_KEY = 'SYNC_MESSAGE_ID';
// 通知を出さない (@silent) メッセージフラグ
const SUPPRESS_NOTIFICATIONS = 1 << 12;

/** 編集トリガーを作成する (既存のものと旧版の変更トリガーは作り直す) */
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
  pushToDiscord();
}

/** tickets / masters の表示値を読み、Webhook で固定メッセージの sync.json を差し替える */
function pushToDiscord() {
  const props = PropertiesService.getScriptProperties();
  const url = props.getProperty('DISCORD_WEBHOOK_URL');
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
    const file = Utilities.newBlob(JSON.stringify(snapshot), 'application/json', 'sync.json');
    const content = `TTM-DB 自動同期 (スプシ編集時にこのメッセージが更新されます): tickets ${rows}行`;
    const id = props.getProperty(MESSAGE_ID_KEY);
    if (id) {
      // 既存の添付は attachments に含めないことで消え、files[0] に置き換わる
      const res = send('patch', `${url}/messages/${id}`, {
        payload_json: JSON.stringify({
          content: content,
          allowed_mentions: { parse: [] },
          attachments: [{ id: 0, filename: 'sync.json' }],
        }),
        'files[0]': file,
      }, true);
      if (res) return;
      // 固定メッセージが削除されていたら作り直す
    }
    const created = send('post', url + '?wait=true', {
      payload_json: JSON.stringify({
        content: content,
        allowed_mentions: { parse: [] },
        flags: SUPPRESS_NOTIFICATIONS,
      }),
      'files[0]': file,
    });
    props.setProperty(MESSAGE_ID_KEY, created.id);
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

/**
 * Webhook に送り、応答のメッセージを返す。レート制限 (429) なら待って再送する。
 * allowMissing なら 404 (メッセージが無い) のとき null を返す
 */
function send(method, url, payload, allowMissing) {
  for (let i = 0; i < MAX_RETRY; i++) {
    const res = UrlFetchApp.fetch(url, {
      method: method,
      payload: payload,
      muteHttpExceptions: true,
    });
    const code = res.getResponseCode();
    if (code >= 200 && code < 300) return JSON.parse(res.getContentText() || '{}');
    if (code === 404 && allowMissing) return null;
    if (code === 429) {
      const body = JSON.parse(res.getContentText() || '{}');
      Utilities.sleep(Math.ceil((body.retry_after || 1) * 1000));
      continue;
    }
    throw new Error(`Discord Webhook エラー ${code}: ${res.getContentText()}`);
  }
  throw new Error('Discord Webhook のレート制限が解除されませんでした');
}
