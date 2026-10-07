# Wi-Fi STA・JPEGブラウザ表示

## 接続と設定

ESP32-P4にはWi-Fi無線がないため、ESP32-C6のESP-Hosted **3.0.9互換FW**を使用します。
確認済み配線に合わせて `sdkconfig.defaults` にSDIO slot 1／4-bit／40 MHzを設定しました。

| 信号 | P4 GPIO |
|---|---:|
| CLK | 18 |
| CMD | 19 |
| D0 / D1 / D2 / D3 | 14 / 15 / 16 / 17 |
| C6 RESET / EN | 54 |

GPIO7/8のカメラSCCB設定とは独立です。CMDとD0〜D3には基板上の外部プルアップが必要です。

`wifi.conf.example` を `wifi.conf` にコピーしてSSID・パスワードを設定してください。

```text
ssid=使用するSSID
password=使用するパスワード
```

- PCとC6が同じLANに接続できる **2.4 GHz AP** を使用します。
- クォートは不要です。`=`より後のスペースは値の一部です。
- SSIDは1〜32バイト。通常のパスフレーズは8〜63バイト。64桁の16進PSKも指定可能です。
- オープンネットワークでは `password=` とします。パスワードありの場合、WPA2以上を要求します。
- ファイルがない／SSIDが空の場合、起動時に明示的な設定エラーで終了します。
- `wifi.conf` はGit管理対象外です。**値はビルド時にファームウェアへ埋め込まれるため、
  ファームウェア・ビルド成果物・詳細ビルドログも機密情報として扱ってください。**
- Hostedの実際のstorage RPCを使い、今回の設定はC6のRAMへ渡します。
  過去にC6へ保存された資格情報を消去する処理ではありません。

SDK・ドライバーはEspressif公式リポジトリ／公式Component Registryから取得します。
隣接する `../esp_hosted` への依存はありません。

- ESP-IDF：`https://github.com/espressif/esp-idf.git` の公式6.1リリースコミット
  `fff9895c82d744c7237be8847347bdd1b07c6643` を `.cargo/config.toml` で指定。
- Hosted：公式Component Registryの `espressif/esp_hosted =3.0.9`。
- センサー／SCCB：公式 `espressif/esp-video-components` のコミット固定（`Cargo.toml`参照）。
- mDNS／CMake utilities：公式Component Registryの固定バージョン。

非公開CSIヘッダーも、リンクする同じ公式SDK内の `esp_driver_cam/csi/src` を直接参照します。
`idf/official_sdk.cmake` がSDKの公式origin URL・固定コミット・変更のない追跡ファイルと、
CSIドライバーのパスを確認します。別コンポーネントによるCSIドライバーの差し替えは拒否します。
`IDF_PATH`が設定されている場合、esp-idf-sysはそのローカルSDKを優先するため、
そのチェックアウトも上記条件を満たす必要があります。環境変数の `IDF_PATH` 自体は変更しません。
SDK自動取得を使う場合はビルド時に `env -u IDF_PATH` を指定し、起動済みSDK環境を外してください。
初回取得にはGitHub・EspressifのRegistry／配布ストレージへのネットワークアクセスが必要です。

HostedのGit取得はComponent Manager 3.1.2のサブモジュールメタデータのハッシュ不一致を
起こすため、検証を無効化せず公式配布アーカイブを使います。以前のチェックアウト
`0ff20e9a8731ee826d4e8199cef15e956a5c1745`より19コミット前の正式版ですが、
現在の4-bit SDIO構成で使う集約送信・storage RPC・通信プロトコルは同一です。
後続の他トランスポート・省電力・C6側修正は含みません。既存C6 FWは書き換えません。
ESP-IDF 6.1にWi-Fi Remoteが含まれるため、別のesp_wifi_remoteコンポーネントは追加しません。

```sh
cargo run --release
```

この環境のsccache-wrapperが利用できない場合のビルド／実行:

```sh
PIP_NO_INDEX=1 RUSTC_WRAPPER= cargo run --release
```

## PCで表示する

起動後、PCのブラウザで次へアクセスします。

**http://esp4-fridge.local/**

通常のシリアル出力は撮影トリガー情報だけに制限しています。IP取得ログは表示しません。
mDNSをサポートしないPC／ネットワークではルーターのDHCP一覧からIPを確認できますが、
**IP直指定では再起動後のDHCPアドレス変更を追従できません**。
その場合はルーターでC6のMACアドレスにDHCP予約を設定してください。
PCとP4の間でクライアント隔離、HTTP遮断、mDNS遮断がある場合はアクセスできません。
同じホスト名の複数台利用には対応していません。

画面は **トリガー時に作成された最新JPEG** を表示します。30FPS動画の配信ではありません。
P4のカメラ撮影設定は1920×1080 RAW10／30FPSのままです。
表示遅延を減らすため、アプリのJPEG品質は80から60に変更しています。
JPEG解像度は1920×1080を維持しますが、圧縮による画質低下があります。
更新検出は200ms間隔、生成後の取得は1回のHTTPリクエストで行います。
トリガーから表示まで約2秒を目標にしていますが、無線速度・再送・画像内容に依存し、
2秒以内を保証する設定ではありません。

- `/` : 外部サービス／ライブラリ不要の表示ページ。
- `/status` : 起動ID、JPEG更新番号、撮影フレーム番号、バイト数。
- `/image.jpg?boot=起動ID&version=更新番号` : 指定した最新JPEG（既存クライアント用）。
- `/latest.jpg` : そのリクエストで選択した最新JPEG。起動ID、更新番号、フレーム番号、
  サイズを `X-Fridge-Boot/Version/Frame/Bytes` ヘッダーで返します。
- `/latest.jpg?boot=表示済み起動ID&version=表示済み更新番号` : 同じ画像なら204、
  更新済みなら新しいJPEGを返します。status取得との競合による409再試行は不要です。
- 最初のJPEGがまだない場合はHTTP 503。
- 旧 `/image.jpg` ではstatus取得後に画像が変わるとHTTP 409を返します。
- ブラウザは `/latest.jpg` で通常200ms間隔に更新を確認し、同じ画像はダウンロードしません。
  画像送信中に新しいトリガーが発生した場合は、現在の送信・デコード完了後に
  最新画像を取得します。転送途中のキャンセルや全トリガーの配信キューはありません。

### 撮影トリガーの通知

画像の上に `撮影トリガーを検出しました（フレーム N）。` を表示します。
検出時に `JpegPublisher::notify_trigger()` で通知情報だけを記録し、JPEGの生成・公開とは
独立して更新します。JPEG busyでスキップされたトリガーも通知対象なので、メッセージは
撮影成功や画像更新完了を意味しません。

`/latest.jpg` の200／204／503応答には `X-Fridge-Trigger` を付けます。
形式は `起動ID（16桁hex）:通知番号（u32）:フレーム番号（u32）`、通知番号0は未検出です。
ブラウザはヘッダー受信時に表示し、JPEG本文の取得・デコード完了を待ちません。
同じ通知は重複表示せず、通信エラー時も最後のメッセージを保持します。
P4再起動後は新しい起動IDで通知番号のリセットを識別します。

追加の通信は行わず、既存の画像ポーリングで最新の通知を取得します。
**画像転送中に発生したトリガーは次のポーリングまで表示が遅れます。**
複数のトリガーが確認の間に発生した場合は最新だけを表示します。
新しいファームウェアへ更新した際は、一度ブラウザを再読み込みして新しいページを取得してください。

## P4再起動・ネットワーク切断時

**すでに開いてあるページ**は通信失敗時も表示済み画像を残し、最大5秒間隔で無制限に再試行します。
リクエスト／本文取得／デコードには10秒の期限があります。オンライン復帰・タブ再表示でも再試行します。
P4が復帰し、新しいトリガーJPEGが作成されるとページ再読込なしで新画像を表示します。
端末起動中のページ初回ロード自体が失敗した場合は、端末復帰後にページを開き直してください。

JPEG更新番号と撮影フレーム番号は起動時にリセットされますが、
NVSの `fridge_wifi/boot` に起動カウンタをコミットし、起動IDを変更することで見落としを防ぎます。
NVS書き込みエラー／カウンタ上限の場合は明示的に起動エラーにします。NVSを自動消去しません。
NVSを手動で消去した場合は起動IDが再利用され得るため、ブラウザを再読込してください。
通常起動につきNVS更新は1回です。

STAのStart／切断イベントは状態更新とワーカー起床に使います。
CPU1の `wifi_reconnect` がHosted RPCで接続し、切断後も概ね5秒間隔で継続して試行します。
JPEG処理スレッドやカメラ取得スレッドからWi-Fi接続RPCを呼びません。
アプリ独自イベントハンドラはRPC／ログを行いませんが、IDFのデフォルトnetifハンドラが
MAC取得などのHosted RPCを行う経路は残ります。
Hosted初期化RPCが失敗した場合、初期化状態が不確定なため同一起動中の再startを禁止します。

## メモリ・スレッド・安全性

`src/wifi.rs` の `JpegPublisher::publish()` がJPEG完成コールバックで画像をPSRAMへコピーします。
送信する画像は不変のArcスナップショットです。エンコーダの再使用される出力ポインタを公開しません。

- FreeRTOS tickは1000Hz。Hostedの1ms TX-credit待ちを実際のブロックにし、
  高優先度タスクで0tick待ちに丸められることを避けます。
  カメラ／JPEGの `vTaskDelay(1)` は1msのブロックになります。
- TCP送信バッファは5760バイトから32768バイトに拡張（接続ごとの送信メモリ使用量が増加）。
- 通常のアプリログは `Capture trigger: frame N, center APL=…, delta=…` のみ。
  JPEG生成・計測・公開、HTTP接続・送信、Wi-Fi接続、Hosted初期化のINFOログは表示しません。
  異常時のWARN／ERRORは診断用に残します。ROM／ブートローダーやアプリ初期化前の起動ログは別です。
- Hostedは公式配布の3.0.9ソースを無改変でコンパイルします。
  空きバッファ待ちで未送信のSDIO集約を保持し続ける独自修正は撤去しました。
  公式実装の待ち条件によっては集約が破棄され、TCP再送により画像配信の遅延が増える可能性があります。
  変更後の転送時間・タイムアウトは実機で再確認してください。HTTPの送信期限は維持しています。
- 定期パケット統計は `CONFIG_ESP_HOSTED_HOST_PKT_STATS=n` で無効です。
  再度診断を有効化する場合、
  公式3.0.9の `credit_stall` は空きバッファ待ちによる集約破棄回数を示します。
  現在のHosted実装ではSW_AGGRのTX `trans`／`out`／`drop` の計数が未実装なので、
  これらが0でも未送信・未破棄とは判断できません。
- `fridge_http` の定期送信進捗ログと計測は削除しました。送信期限と失敗時のWARNは維持します。
- HTTPサーバーはCPU1、優先度4、内部RAMスタック8192バイト。
- 再接続ワーカーはCPU1、優先度3、内部RAMスタック8192バイト。
- 配信画像の合計PSRAM予算は **8MiB**。最新画像、送信中の旧画像、作成中の新画像をすべて数えます。
- 実際の画像サイズだけ確保します。メモリ不足／予算超過時は新画像の公開をスキップし、旧画像を保持します。
  大きな画像では旧画像と新画像を同時に予算内に収められず更新できない場合があります。
- カメラ2面とJPEGエンコーダの約20MiBに対し、配信画像が最大8MiBとネットワーク管理領域が追加されます。
- ストアMutexはネットワーク送信中に保持しません。閲覧者の遅さでJPEG処理を待たせません。
- C6のWi-Fi省電力を `WIFI_PS_NONE` で無効化し、TCP ACK受信の待ち時間を減らします。
  省電力使用時より消費電力が増えます。
- HTTPソケットに `TCP_NODELAY` を設定し、小さいヘッダー送信や分割JPEG送信で
  Nagleと遅延ACKによる待ち合わせを避けます。
- JPEG送信の開始・終了の通常ログは表示しません。送信失敗時はsocketへ渡せた
  バイト数（ヘッダーを含む。ブラウザ受信完了数ではありません）とエラーも記録します。
- HTTPレスポンスの送信に10秒の絶対期限と個別sendの3秒タイムアウトを設定。
  最大4096バイトずつ送るため、遅い閲覧者が送信タスクを無期限に占有しないようにします。
  期限超過の検出はブロッキングsendの終了後なので、最大約3秒余分にかかり得ます。
- 全レスポンスを `no-store` とし、送信後は実際にHTTPセッションを閉じます。
- カメラ／JPEGのWDT監視・idleタスクへの実行時間確保は維持します。

**HTTPに認証・暗号化はありません。同じLANからアクセスできる相手は画像を閲覧できます。**
信頼できるLANに限定し、ポート開放でインターネットに公開しないでください。

## RustとCの役割分担

ネットワーク制御は既存の `esp-idf-sys 0.38.1` から標準APIを直接呼びます。
新しいRust依存クレートは追加していません。

- `src/wifi_nvs.rs`：起動カウンタの読出し・上限確認・書込み・commit。
  ハンドルはRAIIで必ず閉じ、commit成功後だけ新しい起動IDを返します。自動消去はしません。
- `src/wifi_http.rs`：HTTP設定の上書き・サーバー起動・TCP_NODELAY・4096バイト送信制限・
  10秒期限・実際の接続終了。期限の状態はHTTPタスクごとのthread-localで管理し、
  現在の同期URIハンドラーを前提とします。ブロッキングsend中の期限超過の扱いは従来と同じです。
- `src/wifi.rs`：`eh_host_wifi_set_storage(WIFI_STORAGE_RAM)` を直接呼び、C6のRAM保存を指定。
  Hosted 3.0.9の汎用storage hookで代用しません。
- `src/network_defaults.rs`：`WIFI_INIT_CONFIG_DEFAULT()`／`HTTPD_DEFAULT_CONFIG()`相当の
  初期値をRustで明示的に構築します。Wi-FiのOS／暗号関数テーブル、magic、Kconfig由来の値は
  `esp-idf-sys`のバインディングから取得し、ゼロ初期化だけで代用しません。
  `network_defaults.c`は削除しました。全フィールドの明示とESP-IDF 6.1のコンパイル時チェックにより、
  SDK変更時には見直しを要求します。`tskNO_AFFINITY`のみ、bindgenが扱えないキャスト式を
  `bindings.h`のenumでClangに評価させます（Cの実行処理ではありません）。
  SDK更新時は `python3 tests/network_defaults.py` で公式マクロとの対応も確認してください。
- `src/camera_stop.rs`：CSI停止時のDMAコールバック解除とクリティカル区間をRustで実装。
  実SDKの非公開ヘッダーから生成した型と `offset_of!` を使い、構造体レイアウトを手書きしません。
  元の固定タスク／IRQコア上で停止し、エラー時もRAIIでクリティカル区間を解除します。
  ESP-IDF 6.1・ESP32-P4・2コア構成をコンパイル時に確認します。
  `camera_stop.c`と`components/fridge_camera`は削除しましたが、非公開APIへの依存は残ります。

  `idf/`にはヘッダーのみのSDKビルド設定を置きます。`idf/CMakeLists.txt`はSDK依存関係・
  非公開CSIヘッダーの検索パスを設定し、公式SDKの参照先を検査します。
    独自CソースやHostedの修正コピーはコンパイルしません。
  `idf/bindings.h`はRust用バインディング生成に必要です。ESP-IDF・公式センサードライバー・
  Hosted自体のCコードのコンパイルは引き続き必要です。詳細は `idf/README.md` を参照してください。

### esp-idf-svcの検討結果

手元のキャッシュにある `esp-idf-svc 0.53.0` は `esp-idf-hal 0.47` が必須依存で、
HALの `esp-idf-sys = "0.38"` は現在の0.38.1と整合します。P4 Remote Wi-FiとIDF 6の
条件分岐もソースにあります。ただし、この構成でのsvcのビルド・実機動作は未検証です。

**現時点では採用を保留します。** default featuresを外してもHAL依存は残り、HALのI2Cモジュールには
旧 `i2c_param_config`／`i2c_driver_install` APIが含まれます。ソースに含まれるだけで必ず
リンクされるとは限りませんが、以前の新旧I2Cドライバー競合を再発させないことの確認が必要です。
コンフリクトチェックの無効化やLTO回避策を導入する方針ではありません。

導入を試す場合はまず最小構成のリンク・起動を検証し、次にNVS／イベントループなどを段階的に移します。
その際、svcのNVSは `take_with(false)` で自動消去を禁止し、Hosted専用storage RPCは維持します。
HTTPの現在の優先度4・個別タイムアウト3秒・送信期限・接続終了も維持が必要です。
キャッシュ版svcのHTTP設定は優先度5・個別タイムアウト5秒で、単純置換は同じ動作にはなりません。
CSI／ISP／ハードウェアJPEGは引き続きraw APIが必要です。

## 検証

ホスト側には `tests/wifi_store.rs`（所有権・予算・再起動後の更新番号）、
`tests/wifi_nvs.rs`（起動カウンタのcommit・エラー時のクローズ）、
`tests/wifi_http.rs`（絶対期限・部分送信の予算・送信サイズ制限）、
`tests/network_defaults.py`（使用中SDKの公式マクロとRust初期化の全フィールド照合）、
`tests/camera_stop.rs`（CSI停止の呼出し順・コールバック解除・エラー時のロック解除）、
`tests/official_sdk.py`（公式SDKのorigin・コミット・変更・CSI差し替え拒否の模擬テスト）、
`tests/wifi_page.mjs`（再接続・再起動・タイムアウト・更新競合・デコード競合）のテストがあります。

```sh
rustc --edition=2021 --test tests/wifi_store.rs -o target/wifi_store_tests
./target/wifi_store_tests
rustc --edition=2021 --test tests/wifi_nvs.rs -o target/wifi_nvs_tests
./target/wifi_nvs_tests
rustc --edition=2021 --test tests/wifi_http.rs -o target/wifi_http_tests
./target/wifi_http_tests
rustc --edition=2021 --test tests/camera_stop.rs -o target/camera_stop_tests
./target/camera_stop_tests
node --test tests/wifi_page.mjs

python3 tests/network_defaults.py
python3 tests/official_sdk.py
```

ビルド／ホストテストと実機検証は別です。C6との通信、AP接続、PC表示、
閲覧中のP4再起動、DHCP変更、JPEGの色順、CSI停止・再起動は実機で確認してください。
CSI停止のホストテストは模擬SDKで制御フローを確認するもので、実SDKのABIや割込み・DMA動作を検証しません。
