# block-reward-sharing

Solana validator の identity に貯まった block 報酬を、epoch の終わりに振り分ける Rust スクリプトです。
中身は `src/main.rs` の 1 ファイルだけです。

```
                    ┌─ ① vote account を目標残高まで補充
identity の余剰 ────┼─ ② staker へ分配 (Jito Priority Fee Distribution)
                    └─ ③ 残りを余剰アカウントへ
```

cron で 15 分おきに起動しておくと、epoch の終盤に 1 回だけ、1 本のトランザクションで送金します。


## 使い方

### 1. 設定を書き換える

`src/main.rs` の冒頭にある「設定」ブロックを書き換えます。

| 定数                  | 内容                                                     | 既定値      |
| --------------------- | -------------------------------------------------------- | ----------- |
| `RPC_URL`             | 使う RPC                                                 | mainnet-beta |
| `IDENTITY_KEYPAIR`    | validator の identity の keypair のパス                  |             |
| `VOTE_ACCOUNT`        | vote account のアドレス                                  |             |
| `SAVINGS_ACCOUNT`     | 余剰をためておくアカウントのアドレス                     |             |
| `IDENTITY_RESERVE`    | identity に残しておく額                                  | 10 SOL      |
| `VOTE_ACCOUNT_TARGET` | vote account をこの残高まで補充する (0 で補充しない)     | 10 SOL      |
| `SHARE_BPS`           | staker に分配する割合 (5000 = 50%)。残りは余剰アカウントへ | 5000        |
| `RUN_AFTER_SLOT_INDEX`| epoch 内でこの slot を過ぎたら実行する (420,000 なら epoch 終了の約 40 分前) | 420,000     |

余剰アカウントは事前に作っておいてください (少額の SOL を送っておけば OK)。

### 2. 送らずに確認する

```sh
cargo run --release -- --dry-run
```

配分を表示して simulate するだけで、何も送信しません。実行タイミングのチェックも無視するので、いつでも試せます。

### 3. cron に登録する

```sh
cargo build --release
```

```cron
*/15 * * * * cd /path/to/block-reward-sharing && ./target/release/block-reward-sharing >> ~/block-reward-sharing.log 2>&1
```

epoch の終盤 (`RUN_AFTER_SLOT_INDEX` 以降) になるまでは何もせずに終わります。
処理した epoch は `last-epoch` ファイルに記録されるので、同じ epoch に 2 回送ることはありません。


## 配分の順番

identity の残高のうち `IDENTITY_RESERVE` を超えた分 (= 前回からの block 報酬) を、上から順に使います。

1. **vote account の補充** — `VOTE_ACCOUNT_TARGET` に届くまで。余剰が足りなければ出せる分だけ入れ、残りは次の epoch 以降に埋めます
2. **PFD account の rent** — その epoch で最初に分配するときだけ
3. **staker への分配と余剰アカウント** — 残りを `SHARE_BPS` の比率で分けます

1 と 2 を分ける前に引くので、そのコストは staker と同じ比率で負担することになります。


## 出力例

```
[dry-run] 配分の確認と simulate だけ行い、送信はしません
epoch 1097  (slot 170119 / 432000)
→ slot 420000 になるまで待ちます

残高
     12.758643917 SOL  <address>  identity
      9.203048000 SOL  <address>  vote account
      0.001020056 SOL  <address>  余剰アカウント
      0.000000000 SOL  <address>  PFD account (epoch 1097, 未作成)

配分  (identity の余剰 2.758638917 SOL)
      0.796952000 SOL  → vote account に補充
      0.001544320 SOL  → PFD account の作成 (rent)
      0.980071298 SOL  → staker へ分配 (50%)
      0.980071299 SOL  → 余剰アカウントへ

→ simulate 成功 (16427 CU, 583621 bytes)
```


## 仕組み

### staker への分配 (Jito Priority Fee Distribution)

Jito の Priority Fee Distribution (PFD) program に、epoch ごとの PFD account を作って入金します。
epoch が終わると Jito の TipRouter NCN が stake の比率で配分を計算し、各 staker の stake account へ配ってくれます。
こちらがやるのは「作る」と「入金する」の 2 つだけです。

- 配布時に JIP-16 で決められた手数料 (1.5%) が差し引かれます
- PFD account を作れるのは vote account の identity だけです。そのため identity の keypair が必要です

### 安全のために

- **1 本のトランザクションにまとめる** — どれかの送金が失敗すれば全体が取り消されます
- **金額は毎回残高から計算する** — 送信に失敗しても、次の実行で計算し直すだけなので二重に送ることはありません
- **identity の reserve は割らない** — 手数料も含めて `IDENTITY_RESERVE` を下回らないように計算します
- **`--dry-run` は署名せずに simulate する** — そのまま送信できるトランザクションを RPC に渡しません


## 必要なもの

- Rust (バージョンは `rust-toolchain.toml` で指定しています。rustup があれば自動で入ります)

```sh
cargo test   # PFD の命令やアドレスが Anchor / mainnet と一致するかのテスト
```
