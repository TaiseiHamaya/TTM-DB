# TTM-DB (TaskTicketManager-DiscordBot)

Discord でタスクチケットの発行と状態管理を行う Bot です。台帳は Google Sheets、チケットの置き場は Forum チャンネルです。設計は [DESIGN.md](DESIGN.md) を参照してください。

## セットアップ

### 1. Google Sheets を用意する

1. スプレッドシートを作り、チケット台帳用とマスタ用の2シートを用意する。シート名は自由
2. 台帳シートにヘッダを並べる (既定は1行目。行番号は設定で変更可)。ヘッダ名・列順は自由で、独自列の追加も可
3. マスタシートに選択肢を書く (A=key, B=value, C=補足)
   - `status` 行の C列は役割 (`initial` / `in_progress` / `done` / `suspended` / `discarded`)。名称は自由に変更可
   - `assignee` 行は手動で追加するか (C列=Discord ID)、`/sync members` や参加者追加時に Bot が自動で追記する
   - 名称変更・統合は `rename | 旧名 | 新名` の行を追加して `/sync tags` を実行する。tickets の値が新名に移り、masters に無くなったタグは Forum から削除される
4. Google Cloud で Service Account を作成して Sheets API を有効化し、スプレッドシートを Service Account のメールに **編集者** で共有する

### 2. シート名・ヘッダ名・担当者名を設定する

1 で決めた名前を `config.toml` に書く。環境変数でも設定でき、両方ある場合は環境変数が優先される (サーバーごとに変える値は環境変数に書くと便利)。

1. **シート名**: `[sheets]` の `tickets_sheet` と `masters_sheet` に、台帳シートとマスタシートの名前を書く。台帳シートの上にタイトル行などを置く場合は、ヘッダの行番号を `header_row` に書く (既定は1行目)

   ```toml
   [sheets]
   tickets_sheet = "チケット"
   masters_sheet = "マスタ"
   header_row = 3   # 1〜2行目はタイトルなど。3行目がヘッダ、4行目以降がチケット
   ```

   環境変数の場合: `TTM_TICKETS_SHEET=チケット` / `TTM_MASTERS_SHEET=マスタ` / `TTM_HEADER_ROW=3`

2. **ヘッダ名**: `[columns.names]` に「Bot の論理名 = 台帳シートのヘッダ名」を全列分書く (`config.toml` に全15列のひな形あり)

   ```toml
   [columns.names]
   ticket_id = "チケットID"
   title = "件名"
   assignee = "担当者"
   # ...残りの列も同様に
   ```

   環境変数の場合: `TTM_COL_<論理名の大文字>=ヘッダ名` (例: `TTM_COL_TITLE=件名`)

3. **担当者名 (Discord ↔ スプシ)**: `[members]` に「Discord ユーザーID = スプシに書く名前」をメンバー全員分書く。キーはユーザー名 (@ の後ろのハンドル) でも可だが、変更されても崩れない ID を推奨

   ```toml
   [members]
   "123456789012345678" = "田中"
   "234567890123456789" = "佐藤"
   ```

   環境変数の場合: `TTM_MEMBERS=123456789012345678=田中,234567890123456789=佐藤`

   設定に無いメンバーは、masters の assignee 行の名前、それも無ければ Discord のサーバー表示名で記録される。設定と masters の名前が食い違っていると `/sync check` で報告される

### 3. Discord を用意する

1. Developer Portal で Bot を作成し、**SERVER MEMBERS INTENT** と **MESSAGE CONTENT INTENT** を ON にする (参加者同期と画像検知に必要)
2. `bot` と `applications.commands` スコープで招待する。権限は次の通り: チャンネル管理 (タグ管理)、スレッド管理、メッセージ送信 / スレッドでメッセージ送信、公開スレッド作成、リアクション追加、埋め込みリンク、メッセージ履歴を読む
3. Forum チャンネル `#tickets` を作成する。削除権限は管理者のみにする
4. (任意) 警告投稿用のテキストチャンネルを `config.toml` の `discord.alert_channel_id` に設定する

### 4. スプシ編集の即時反映を設定する (Pub/Sub と GAS)

人がスプシを編集すると、GAS (`gas/sync.gs`) が Google Cloud Pub/Sub のトピックに「編集があった」合図を送ります。Bot はサブスクリプションから合図を受け取り、Sheets API でシートを読み直して、変わったチケットを Forum Post に反映します。続けて編集された場合は、1.5秒待ってまとめて1回だけ読みます。Bot から Pub/Sub に接続しに行くだけなので、Bot を外部に公開する必要はありません。

**Google Cloud (Service Account と同じプロジェクト)**

1. Pub/Sub API を有効にする
2. トピックを作る (例: `ttm-db-sheet-edited`)。「デフォルトのサブスクリプションを追加する」はオフでよい
3. そのトピックにサブスクリプションを作る (例: `ttm-db-bot`)。配信タイプは **プル**、他は既定のまま
4. 権限を付ける
   - サブスクリプションに、Bot の Service Account を **Pub/Sub サブスクライバー** で追加する
   - トピックに、スプシの GAS を実行する Google アカウント (トリガーを作る人) を **Pub/Sub パブリッシャー** で追加する。プロジェクトのオーナー・編集者なら不要
   - GAS は API の利用をこのプロジェクトに付けて送るため、その Google アカウントにはプロジェクトの Service Usage ユーザー権限 (`serviceusage.services.use`) も要る。オーナー・編集者なら付いている

gcloud の場合:

```sh
gcloud services enable pubsub.googleapis.com
gcloud pubsub topics create ttm-db-sheet-edited
gcloud pubsub subscriptions create ttm-db-bot --topic=ttm-db-sheet-edited
gcloud pubsub subscriptions add-iam-policy-binding ttm-db-bot \
  --member=serviceAccount:<Service Account のメール> --role=roles/pubsub.subscriber
```

**Bot**

5. `config.toml` の `sync.pubsub_subscription` に `projects/<プロジェクトID>/subscriptions/ttm-db-bot` を書く

**GAS**

6. スプシの 拡張機能 > Apps Script に `gas/sync.gs` を貼り、先頭の `TICKETS_SHEET` / `MASTERS_SHEET` を 2 で決めたシート名にする
7. プロジェクトの設定で「`appsscript.json` マニフェスト ファイルをエディタで表示する」をオンにし、`appsscript.json` を `gas/appsscript.json` の内容にする (Pub/Sub のスコープを追加するため)
8. プロジェクトの設定 > スクリプト プロパティに `PUBSUB_TOPIC` = `projects/<プロジェクトID>/topics/ttm-db-sheet-edited` を追加する
9. エディタで `setupTrigger` を1回実行し、権限を承認する (編集トリガーが作られ、旧版のトリガーは削除される)
10. エディタで `testPublish` を実行し、Bot のログに「スプシの編集を反映」が出れば完了

Bot が Sheets API で書き込んだ変更 (チケット発行など) では合図は送られません。行の削除など、セルの編集にならない変更は `/sync all` で反映してください。受信や反映に失敗し続けると、`discord.alert_channel_id` に警告が投稿されます。

旧版 (Discord Webhook に sync.json を送る方式) から移行する場合は、スクリプト プロパティの `DISCORD_WEBHOOK_URL` / `SYNC_MESSAGE_ID`、`config.toml` の `discord.sync_webhook_id` / `discord.sync_channel_id`、同期用の Webhook とチャンネルは不要なので削除してください。

### 5. 起動

```sh
cp deploy/ttm-db.env.example .env   # 値を埋める
set -a; . ./.env; set +a
cargo run --release
```

### 6. Oracle Cloud (Ampere A1, Ubuntu)

```sh
# VM 上でビルド (aarch64)
cargo build --release
sudo useradd -r -s /usr/sbin/nologin ttmdb
sudo mkdir -p /opt/ttm-db/data /etc/ttm-db
sudo cp target/release/ttm-db config.toml /opt/ttm-db/
sudo chown -R ttmdb /opt/ttm-db
sudo cp deploy/ttm-db.env.example /etc/ttm-db/ttm-db.env  # 編集する
sudo cp service-account.json /etc/ttm-db/ && sudo chmod 600 /etc/ttm-db/*
sudo chown ttmdb /etc/ttm-db/service-account.json
sudo cp deploy/ttm-db.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ttm-db
journalctl -u ttm-db -f
```

## コマンド

| コマンド | 説明 |
|---|---|
| `/ticket create` | フォームでタイトル・詳細・期限・親・画像を入力し、続けて表示されるメッセージで種類・担当者・優先度を選んで発行。チケットの Post 内で実行すると親が自動入力される |
| `/ticket status id status` | 進行度を変更 (ボタンの代替) |
| `/list [status] [category] [assignee] [priority]` | 一覧 (最大20件) |
| `/search query` | タイトル・詳細・ID の部分一致検索 |
| `/sync ticket id` | 指定チケットを Sheets から即時同期 |
| `/sync check` | マスタ不整合・必須欠落・ID重複などを検査 |
| `/sync all` / `tags` / `members` | 管理者のみ。Sheets を読み直して全件強制同期 / タグ名変更反映 / 参加者取込 |

## 注意点

- `image_urls` に保存する画像 URL は Discord CDN の URL で、一定時間で失効します。元の画像は Post 内のメッセージに残ります
- 初回起動後 (`data/sync_state.json` がまだ無いとき) にスプシ編集の合図で最初に同期するときは、既存行を同期済みとみなして記録だけ行います。Discord に反映が必要なら `/sync all` を実行してください
- Bot が停止中にスプシを編集した場合、合図はサブスクリプションに残り (既定で7日間)、Bot の起動後に反映されます
