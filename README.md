# synology-photos-thumbgen

Synology Photos（Personal Space）でサムネイル未生成の HEVC 動画（DJI Osmo Pocket / Osmo Action など）や HEIC 画像を自動検出し、`ffmpeg` / `pillow-heif` でプレビュー画像（`thumb_sm` / `thumb_m` / `thumb_xl`）を生成して Synology Photos の公式 API（`SYNO.Foto.Upload.ConvertedFile`）経由で登録する Docker サービスです。

## 特徴
- **Synology Image Assistant と同じ公式 API を使用**: `@eaDir` や PostgreSQL DB を直接書き換えず、`SYNO.Foto.Upload.ConvertedFile` 経由でサムネイルをアップロードします。
- **ローカル高速読み取り**: ホストの写真ディレクトリ（`PHOTOS_HOST_DIR`、例: `/volume1/homes/<username>/Photos`）を読み取り専用（`:ro`）でマウントし、ローカルから直接 1 フレーム抽出するため数 GB の動画を HTTP ダウンロードしません。
- **サムネイルのみ生成**: 省容量・高速処理のため、H.264 動画の再エンコード（`film_h264`）は行いません。
- **inotify リアルタイム検知＋保険ポーリング**: `/photos` 配下（`@eaDir` 等を除く）を Linux `inotify` で監視し、新規動画・画像の追加後すぐ（デバウンス 15 秒＋必要に応じ 45 秒後に追確認）処理します。取りこぼし防止の定期チェックも 1800 秒（30 分）おきに併用します。
- **2FA（OTP）対応・Auto Block 保護**: 認証エラー時は再試行せず即時停止します。2FA 有効時は初回 `auth` で `device_id` を `./data/state.json` に保存します。

## セットアップ手順

1. `.env.example` をコピーして `.env` を作成し、権限 `600` を設定したうえで各項目を編集します。
   ```bash
   cp .env.example .env
   chmod 600 .env
   ```
   `.env` に以下を設定してください（`PUID` / `PGID` は NAS 上で `id` コマンドを実行して確認できます）。
   - `DSM_USERNAME`: DSM ユーザー名
   - `DSM_PASSWORD`: DSM パスワード
   - `PHOTOS_HOST_DIR`: ホスト側の写真フォルダ（例: `/volume1/homes/<username>/Photos`）
   - `PUID` / `PGID`: ファイル所有ユーザーの UID / GID

2. （2要素認証が有効な場合のみ）初回デバイス登録を行います。
   ```bash
   docker compose run --rm thumbgen auth
   ```
   OTP コードの入力を求められたら入力します。`./data/state.json` に `device_id` が保存されます。

3. 対象一覧のドライラン確認と 1 件テストを行います。
   ```bash
   docker compose run --rm thumbgen once --dry-run
   docker compose run --rm thumbgen once --limit 1
   ```

4. 常駐起動します（`inotify` リアルタイム検知 ＋ 30 分おきの保険ポーリング）。
   ```bash
   docker compose up -d
   docker compose logs -f
   ```

## オフライン検証コマンド

DSM に接続せずローカルファイル単体でサムネイル生成を確認できます。
```bash
docker compose run --rm thumbgen test-local /photos/path/to/video.mp4
```
