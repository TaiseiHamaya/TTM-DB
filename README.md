# TicketManager

Discord でタスクチケットの発行と状態管理を行う Bot です。台帳は Google Sheets、チケットの置き場は Forum チャンネルです。設計は [DESIGN.md](DESIGN.md) を参照してください。

## セットアップ

### 1. Google Sheets を用意する

1. スプレッドシートを作り、チケット台帳用とマスタ用の2シートを用意する。シート名は自由
2. 台帳シートにヘッダを並べる (既定は1行目。行番号は設定で変更可)。ヘッダ名・列順は自由で、独自列の追加も可
3. マスタシートに選択肢を書く (A=key, B=value, C=補足)
   - `status` 行の C列は役割 (`initial` / `in_progress` / `done` / `suspended` / `discarded`)。名称は自由に変更可
   - `assignee` 行は手動で追加するか (C列=Discord ID)、`/sync members` や参加者追加時に Bot が自動で追記する
   - 名称変更・統合は `rename | 旧名 | 新名` の行を追加して `/sync tags` を実行する。tickets の値が新名に移り、masters に無くなったタグは Forum から削除される
4. GCP で Service Account を作成して Sheets API を有効化し、スプレッドシートを Service Account のメールに **編集者** で共有する

### 2. シート名・ヘッダ名・担当者名を設定する

1 で決めた名前を `config.toml` に書く。環境変数でも設定でき、両方ある場合は環境変数が優先される (サーバーごとに変える値は環境変数に書くと便利)。

1. **シート名**: `[sheets]` の `tickets_sheet` と `masters_sheet` に、台帳シートとマスタシートの名前を書く。台帳シートの上にタイトル行などを置く場合は、ヘッダの行番号を `header_row` に書く (既定は1行目)

   ```toml
   [sheets]
   tickets_sheet = "チケット"
   masters_sheet = "マスタ"
   header_row = 3   # 1〜2行目はタイトルなど。3行目がヘッダ、4行目以降がチケット
   ```

   環境変数の場合: `TM_TICKETS_SHEET=チケット` / `TM_MASTERS_SHEET=マスタ` / `TM_HEADER_ROW=3`

2. **ヘッダ名**: `[columns.names]` に「Bot の論理名 = 台帳シートのヘッダ名」を全列分書く (`config.toml` に全15列のひな形あり)

   ```toml
   [columns.names]
   ticket_id = "チケットID"
   title = "件名"
   assignee = "担当者"
   # ...残りの列も同様に
   ```

   環境変数の場合: `TM_COL_<論理名の大文字>=ヘッダ名` (例: `TM_COL_TITLE=件名`)

3. **担当者名 (Discord ↔ スプシ)**: `[members]` に「Discord ユーザーID = スプシに書く名前」をメンバー全員分書く。キーはユーザー名 (@ の後ろのハンドル) でも可だが、変更されても崩れない ID を推奨

   ```toml
   [members]
   "123456789012345678" = "田中"
   "234567890123456789" = "佐藤"
   ```

   環境変数の場合: `TM_MEMBERS=123456789012345678=田中,234567890123456789=佐藤`

   設定に無いメンバーは、masters の assignee 行の名前、それも無ければ Discord のサーバー表示名で記録される。設定と masters の名前が食い違っていると `/sync check` で報告される

### 3. Discord を用意する

1. Developer Portal で Bot を作成し、**SERVER MEMBERS INTENT** と **MESSAGE CONTENT INTENT** を ON にする (参加者同期と画像検知に必要)
2. `bot` と `applications.commands` スコープで招待する。権限は次の通り: チャンネル管理 (タグ管理)、スレッド管理、メッセージ送信 / スレッドでメッセージ送信、公開スレッド作成、リアクション追加、埋め込みリンク、メッセージ履歴を読む
3. Forum チャンネル `#tickets` を作成する。削除権限は管理者のみにする
4. (任意) 警告投稿用のテキストチャンネルを `config.toml` の `discord.alert_channel_id` に設定する

### 4. スプシの GAS を設定する (スプシ → Discord の同期)

Bot はスプシを定期取得しません。スプシが編集されると GAS が tickets / masters の内容を Discord Webhook に投稿し、Bot がそれを受けて Forum Post に反映します。

1. Discord に同期用のテキストチャンネル (例: `#ticket-sync`、Bot 以外は閲覧不要) を作り、チャンネル設定 > 連携サービス > ウェブフックで Webhook を作成して URL をコピーする
2. Webhook URL `https://discord.com/api/webhooks/<ID>/<トークン>` の `<ID>` を `config.toml` の `discord.sync_webhook_id` に書く。Bot はこの Webhook の投稿だけを同期データとして受け付ける
3. スプシの 拡張機能 > Apps Script に `gas/sync.gs` を貼り、先頭の `TICKETS_SHEET` / `MASTERS_SHEET` を 2 で決めたシート名にする
4. プロジェクトの設定 > スクリプト プロパティに `DISCORD_WEBHOOK_URL` = 1 の URL を追加する
5. エディタで `setupTrigger` を1回実行し、権限を承認する (変更トリガーが作られる)

以降、スプシを編集するたびに同期チャンネルへ投稿され、Bot が反映すると ✅、失敗すると ⚠ のリアクションが付きます。メニュー「TicketManager > Discord へ同期」で手動送信もできます。Bot が Sheets API で書き込んだ変更ではトリガーは動きません。

### 5. 起動

```sh
cp deploy/ticket-manager.env.example .env   # 値を埋める
set -a; . ./.env; set +a
cargo run --release
```

### 6. Oracle Cloud (Ampere A1, Ubuntu)

```sh
# VM 上でビルド (aarch64)
cargo build --release
sudo useradd -r -s /usr/sbin/nologin ticketbot
sudo mkdir -p /opt/ticket-manager/data /etc/ticket-manager
sudo cp target/release/ticket-manager config.toml /opt/ticket-manager/
sudo chown -R ticketbot /opt/ticket-manager
sudo cp deploy/ticket-manager.env.example /etc/ticket-manager/ticket-manager.env  # 編集する
sudo cp service-account.json /etc/ticket-manager/ && sudo chmod 600 /etc/ticket-manager/*
sudo chown ticketbot /etc/ticket-manager/service-account.json
sudo cp deploy/ticket-manager.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ticket-manager
journalctl -u ticket-manager -f
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
- 初回起動後 (`data/sync_state.json` がまだ無いとき) に最初に届いたスプシの送信内容は、既存行を同期済みとみなして記録だけ行います。Discord に反映が必要なら `/sync all` を実行してください
- Bot が停止中にスプシを編集した場合、その変更は次にスプシが編集されたとき (またはメニューの手動送信・`/sync all`) に反映されます
