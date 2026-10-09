# TTM-DB 設計書

## 1. 前提

- Discord Bot でタスクチケットの発行と状態管理を行う
- 言語は Rust とする
- 台帳は Google Sheets とする。スプシ側で項目変更が高頻度である前提で設計する
- 種類は7件、担当者はサーバー参加者で表示名そのまま、進行度は6種とする
- 期限と優先度は双方必須項目として追加する
- 通知は DM なしとし、担当者メンションのみとする
- リマインド通知はなしとする
- ホスティングは Oracle Cloud Always Free に決定済みとする

## 2. アーキテクチャ

```text
Discord <-> Bot -> Google Sheets
Google Sheets (GAS) -> Discord Webhook -> Bot
```

- Discord は入力と表示と通知を担う
- Bot は Slash Commands と Modal と Button と Forum タグ操作と Sheets API を担う
- Sheets はチケット台帳とマスタ定義を担う。全状態の正本とする
- Sheets の GAS は人が編集したときに Discord Webhook へシート内容を送る。Bot はこれを受けて Discord に反映する。/sync 実行時は Bot が Sheets を読んで反映し、記録として sync.json を投稿する

### 技術候補

- twilight 0.17 (twilight-gateway, twilight-http, twilight-model, twilight-util) を用いる。Modal 内の選択メニュー (Label) とファイルアップロードに対応しているため採用した
- Sheets API は reqwest で直接呼び出し、認証は yup-oauth2 による Service Account 認証とする。非同期ランタイムは tokio とする
- 設定項目は DISCORD_TOKEN、GUILD_ID、FORUM_CHANNEL_ID、SPREADSHEET_ID、GOOGLE_SA_JSON とする
- マスタと列定義はコード直書きを禁止する。config.toml と Sheets マスタシートから起動時に読み込み、以降は GAS から送られる内容で更新する

## 3. チャンネル設計

Forum チャンネル #tickets 1つを置き場とする。

- Forum Post 1件をチケット1件とする。スレッド相当とし、別途スレッドは作らない
- 完了しても削除とクローズはしない。中断と破棄と完了は Locked で残す。チケットが無くならないことを担保する
- Text チャンネルとスレッド案は不採用とする。理由はタグが使えず、アーカイブで一覧から消え、検索が弱いためである

### Forum タグ構成

進行度5種と種類6件と優先度4値と担当者5人相当の合計20タグで Forum 上限20に収まるため、全てタグ化可能である。進行度からチェック待ちを外し、種類のバグと違和感を統合して優先度の枠を確保した。

- 進行度5種は次の通りとする。未着手、着手中、完了、中断、破棄
- 種類6件は次の通りとする。実装、アセット、エンジン、演出、企画、バグ・違和感
- 担当者はサーバー参加者をそのまま用いる。表示名変更なしとする。人が増えたらタグ上限を超えるため、6人目以降はタグ追加せず /list 担当者検索に切替える運用とする
- 優先度4値は次の通りとする。緊急、高、中、低
- 期限はタグ化しない。本文 Embed と Sheets 列で対応する
- タグは進行度、種類、優先度、担当者の順に作成し、上限を超える分 (6人目以降の担当者) はタグ化しない。1 Post に付くタグは4つで、Discord の上限5以内である
- /sync tags は masters に無くなったタグ (廃止した進行度、統合で旧名になった種類など) を Forum から削除して枠を空ける

## 4. Sheets スキーマ

1行を1チケットとする。ticket_id をキーに Discord post_id と紐付ける。

### tickets シート必須列

| 列名 | 必須 | 説明 | 例 |
|---|---|---|---|
| ticket_id | 必須 | Bot 発番による連番 | T-0001 |
| discord_post_id | 必須 | Forum Post ID | 123 |
| title | 必須 | タイトル | ログインできない |
| body | 必須 | 詳細 | 再現手順を記載 |
| status | 必須 | 進行度5種のいずれか | 着手中 |
| category | 必須 | 種類6件のいずれか | バグ・違和感 |
| assignee | 必須 | 担当者表示名 | サーバー表示名 |
| reporter | 必須 | 発行者表示名 | サーバー表示名 |
| priority | 必須 | 優先度4値のいずれか | 高 |
| due_date | 必須 | 期限 | 2026-10-10 |
| started_at | 任意 | 着手日 | 2026-10-09 |
| completed_at | 任意 | 完了日 | 2026-10-12 |
| parent_id | 任意 | 親チケット ID | T-0000 |
| image_urls | 任意 | CDN URL のカンマ区切り | https://example.com/a.png |
| created_at | 必須 | 作成日時 | ISO8601形式 |
| updated_at | 必須 | 更新日時 | ISO8601形式 |
| discord_url | 必須 | Post URL | https://example.com/post |

- ticket_id は Bot 側採番とし、Sheets ユニークチェックで競合回避する
- assignee と reporter は名前だけを保存する。メンションと権限判定に使う Discord ID は、config.toml の [members] と masters の assignee 行 (C列) から名前で引く。発行時に未登録の担当者・発行者は masters に自動で追記する
- image_urls は Discord CDN URL を保存する
- priority は緊急、高、中、低の4値とする

### masters シート

選択肢は masters シートで管理し、Bot は起動時に読み込む。以降は masters 編集時に GAS から送られる内容で更新する。

```text
masters の A列と B列に key と value を置く
status 行に 未着手、着手中、完了、中断、破棄を置く
category 行に 実装、アセット、エンジン、演出、企画、バグ・違和感を置く
assignee 行に サーバー表示名と Discord ID 対応を置く
priority 行に 緊急、高、中、低を置く
```

- Discord 側 Select 選択肢と Forum タグはこのマスタから生成する。コード改修なしで追加と名称変更を可能にする
- 名称変更時は旧名から新名への移行を Bot の /sync で Forum タグへ反映する
- 担当者はサーバー参加者と同期する。参加者追加時は masters 追記と Forum タグ追加で対応する

### スプシ変更耐性方針

- 列はインデックスではなくヘッダ名で参照する。列順入替と列追加は許容する
- 未知列は Bot が無視して保持する。上書き消去はしない。行更新は取得と差分マージと更新のみで行う
- 必須列の削除とリネームは破壊的操作とする。Bot が起動時バリデーションで検出し、Discord に警告投稿して同期停止する
- 選択肢の値変更は masters 経由のみ許可する。tickets 直書きのtypoは /sync check で検出して報告する

## 5. 発行フロー

Discord の Modal は部品5つまでで、Modal の送信に続けて別の Modal を開くこともできないため、Modal と自動表示のメッセージの2段構成とする。

1. /ticket create (引数なし) で Modal を開く。項目はタイトル、詳細、期限、親チケットID、画像とする。期限は1週間後の日付を初期値とし、親チケットID は Forum Post 内で開いた場合にその Post のチケットを初期値とする。画像はファイルアップロードで最大10枚、任意とする
2. Modal を送信すると、本人だけに見えるメッセージが自動で表示される。種類と優先度は masters から生成した選択メニュー、担当者はサーバー参加者の選択メニューで選び、「発行」を押す。「フォームを修正」で Modal を入力済みの状態で開き直せる。入力途中の内容は Bot のメモリに30分保持する
3. Bot 処理は次の順とする
   1. マスタ照合と期限日付形式でバリデーションし、Sheets へ append する。status 初期値は未着手とする
   2. Forum に Post 作成する。タイトルは「タイトル [ticket_id]」とし、進行度は含めない (スレッド名の変更は Discord のレート制限が厳しく、進行度のたびに変えるとすぐ制限に当たるため)。進行度・種類・優先度・担当者のタグを付与し、本文に担当者メンションと期限と優先度と親子リンクを記載する
   3. Modal で添付された画像を Post 内に投稿し直し、その URL を image_urls に記録する
4. 発行後に Post へ画像を投稿した場合も Bot が添付検知し、image_urls 追記と Sheets 更新を行う
5. 担当者へメンション通知を行う。DM は行わない

### 親子関係

- Modal の親チケットID 入力は T-123形式とする
- チケットの Forum Post 内で /ticket create を実行した場合は、その Post のチケットを親チケットID 欄に自動入力する。消せば親なしで発行できる
- Sheets parent_id 列に保存する
- 子作成時に親 Post へ子のリンクを自動投稿する
- 子側に親への移動ボタンを設置する

## 6. 状態遷移と通知

進行度は未着手から着手中へ、着手中から完了へ進む。完了から着手中への差戻し (修正依頼) を許す。中断と破棄はどの状態からも移行可とする。チェック待ちは廃止し、確認は完了後の差戻しで扱う。

チケット先頭メッセージに常設ボタンを置く。

```text
着手中にする、完了にする、中断と破棄
```

- 押下時は担当者か発行者かで権限チェックし、Sheets status 更新と Forum タグとタイトル更新を行い、スレッド内に担当者宛の状態変更通知を投稿する
- 発行と完了と差戻しの通知はこれで網羅する
- ボタン押下不可時の代替として /ticket status を用意する。指定例は id T-0001 と status 完了である

## 7. 一覧と検索

- 一覧性は Forum ガイドビューによるタグ絞り込みと /list による Embed 20件表示と Sheets リンクで確保する。/list 絞り込み条件は進行度、種類、担当者、優先度とする
- 検索は /search により Sheets 読みで title と body と ticket_id の部分一致を行う。Discord Forum 検索と併用する
- チケット消失防止として Forum は完了時も消さず、中断と破棄も残し、アーカイブしない運用とする。Delete 権限は管理者に絞る

## 8. 同期方針

スプシ側変更が高頻度のため、双方向同期とする。

- 通常は Discord 操作時に即 Sheets 更新し、Sheets 編集時はスプシの GAS から Discord へ送って反映する。Bot から Sheets への定期取得 (ポーリング) は行わない
- GAS はインストール型の編集トリガー (onEdit) で tickets と masters の表示値を読み、JSON 添付として Discord Webhook で送る。Bot は設定した Webhook ID の投稿だけを受け付け、内容ハッシュで差分検出し、Forum タイトルとタグと先頭 Embed を更新する
- 変更トリガー (onChange) は Bot の API 書き込みでも動き、チケット発行のたびに送信されるため使わない。編集トリガーは API 書き込みでは動かないため、送信はループしない。GAS が読んだ時刻より後に Bot が反映したチケットと、順序が入れ替わって届いた古い送信内容は適用しない
- 編集トリガーでは新しい投稿をせず、同期チャンネルの固定メッセージ1件の sync.json を Webhook のメッセージ編集で差し替える。Bot は MESSAGE_UPDATE で受けて反映する。固定メッセージの ID は GAS のスクリプト プロパティに保存し、消えていたら作り直す。/sync ticket と /sync all の実行時は Bot が Sheets を読んで GAS と同じ形式の sync.json を同期チャンネル (discord.sync_channel_id) に投稿し、記録として残す。Bot 自身の投稿は同期データとして処理しない
- 手動として /sync を用意する。ticket_id 指定で即時同期し、check 付きでマスタ不整合と必須欠落を検査する
- 競合解決は updated_at が新しい方を勝ちとする Last-Write-Wins とする。Discord 編集中に Sheets が先に更新されていたら Discord 側操作を拒否して再読込誘導する
- 同時発番対策として ticket_id 発番は Sheets 再読込とユニーク確認後に確定する。重複時はサフィックス付与して警告投稿する

## 9. 非機能と運用

- 認証情報として Bot Token と Service Account JSON は環境変数と Secret 管理とし、リポジトリに含めない
- 権限として Bot に Forum 作成とタグ管理とメッセージ送信権限を付与する。チケット削除権限は管理者のみとする
- ログとして ticket_id と discord_post_id と Sheets row を構造化ログに出力し、3者トレース可能にする
- エラー時は Discord Post に同期失敗リアクションを付け、リトライ用 /sync へ誘導する
- リマインド通知は行わない。期限は表示と絞り込みのみとする

### ホスティング

- Oracle Cloud Always Free を用いる
- 無料内容は Ampere A1 で合計 2 OCPU と合計 12 GB メモリまでである
- 月換算で 1500 OCPU時間と 9000 GB時間まで無料である。1 OCPU と 6 GB の VM を1台常時起動なら枠内に収まる
- ブートボリュームは合計 200 GB まで無料である。本Botは 50 GB もあれば十分である
- 条件はホームリージョンでのみ作成可である。東京と大阪は空きが少なく確保困難なことがある
- 超過分は削除または課金の対象となるため、2 OCPU と 12 GB を超えない構成にすること
- 手順は OCI アカウント作成と VCN 作成と Ubuntu イメージ選択と systemd 常駐である
- Sheets API 自体は無料枠で足りる
