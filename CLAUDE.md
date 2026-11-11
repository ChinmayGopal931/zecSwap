# ZecSwap: notes for coding agents

Non-custodial swaps of shielded ZEC (Ironwood pool) for USDC, both ways. The deposit address's
spend key is split `±(e + z)`: the maker holds `e`, the user holds `z`. An Ethereum contract pays
out against whichever half is revealed, checking it on Pallas, into the user's private Railgun
balance, with no user account on-chain. Wallets also take public USDC at deposit addresses of
their own and shield it into the same balance through the relayer (`docs/deposits.md`). The
contract can still pay an account (the Base route, shelved for the app, still tested). Clients:
Veylor, a Vizor fork with Private USD, and the Zapp Android wallet.

This repo is the protocol and the user's side: the contracts, the crates wallets build on, and the
docs wallets follow. The maker, the relayer and the token issuer are services kept elsewhere.
Nothing here depends on them, and a wallet takes nothing from them on trust.

Design docs: `docs/reverse-flow.md`, `docs/deposits.md`, `docs/railgun-sends.md`, `docs/tokens.md`.

## Layout

| Path | Role |
|---|---|
| `crates/zecswap-core` | Pure crypto: shares, proofs of knowledge, the joint account (UFVK/UA), seed-derived keys (per-swap auth key, deposit addresses in `deposit.rs`), EIP-712 and EIP-3009 signing (`auth.rs`, which keeps the lock contract's digests for swaps still open there), PCZT signing with the combined key |
| `crates/zecswap-railgun` | Railgun keys and `0zk` addresses from the seed, the shield notes payouts go to, fee notes as their receiver reads them; `engine/` checks them against Railgun's engine and wallet SDK (Node: `check.cjs`, `balance.cjs`, `send.cjs`) |
| `crates/zecswap-chain` | Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the Ethereum client (`evm`: settlement and sends, `judge.rs` for claims, reverse refunds and readies, `deposits.rs`, `railgun.rs`, `funding`) |
| `crates/zecswap-api` | The maker's and relayer's wire types (serde), shared with wallets; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (open, verify on-chain, deposit, claim or refund), forward and reverse (`reverse.rs`), and a wallet's deposits (`Depositor`): the reference the apps mirror |
| `crates/zecswap-tokens` | Privacy Pass tokens (RFC 9577/9578) an accept spends: the wallet's side, and with `server` the check a service makes of them (`docs/tokens.md`) |
| `crates/zecswap-jni` | The Android wallet's bindings, `libzecswap.so` (`scripts/build-android.sh`) |
| `crates/zecswap-cli` | Testnet wallet and user CLI (`init`, `status`, `send`, `send-many`, `swap`, `swap --relayer`, `deposit`) |
| `contracts` | Foundry: `src/ZecSwap.sol` (the windows contract), `test/deployed/ZecSwapLocks.sol` (the lock contract, for migration tests), `src/ShieldVault.sol` (per-swap payout vaults), `src/Deposits.sol`, `src/Pallas.sol`, `src/Token.sol`; tests incl. `test/fork` and Rust-made vectors in `test/vectors`; `test/utils/TestToken.sol` is testnet's tUSD |
| `deployments/` | The mainnet contracts as deployed (`scripts/deploy-contract.sh` writes them) |
| `scripts/` | `build-android.sh` (the Android libraries), `deploy-contract.sh` (a mainnet contract) |

## Commands

```sh
cargo test --workspace                                  # offline tests, plus EVM tests on local anvils (needs `forge build`)
cargo clippy --workspace --all-targets; cargo fmt --all
(cd contracts && forge test)                            # FOUNDRY_DISABLE_NIGHTLY_WARNING=1 hides the nightly banner
(cd contracts && FOUNDRY_PROFILE=soak forge test --no-match-path 'test/{fork,ffi}/*')  # ~1 h: 4096×256 stateful runs, 65536 fuzz runs; the coverage corpus in cache/ carries over
# Mutation testing (pip install slither-analyzer; solc 0.8.28 on PATH) edits sources in place: run it on a copy of contracts/
slither-mutate src --test-dir test --test-cmd 'FOUNDRY_FUZZ_RUNS=128 FOUNDRY_INVARIANT_RUNS=32 FOUNDRY_INVARIANT_DEPTH=64 forge test' --timeout 180
(cd contracts && ETH_RPC_URL=… forge test --match-path 'test/fork/*' --isolate -vv)   # real Railgun, USDC and sanctions oracle
cargo build --release -p zecswap-core --example evm_vectors && (cd contracts && FOUNDRY_PROFILE=ffi forge test)
cargo run -p zecswap-cli -- --data-dir .testnet/user status   # the funded test wallet
cargo run -p zecswap-client --example vectors           # known answers for the Android port
# Railgun's engine and wallet SDK (cd crates/zecswap-railgun/engine && npm ci):
node vectors.cjs > ../tests/engine-vectors.json         # regenerate the Rust tests' known answers
node deposit-vectors.cjs > ../../zecswap-core/tests/deposit-vectors.json   # deposit addresses as ethers derives them
cargo run -p zecswap-railgun --example note | node crates/zecswap-railgun/engine/check.cjs
ETH_SEPOLIA_RPC_URL=… node balance.cjs <seed-file> [from-block]   # a wallet's Railgun balance by screening status
RAILGUN_NETWORK=mainnet ETH_RPC_URL=… node balance.cjs <seed-file> [from-block]   # the same on Ethereum
```

## How it behaves (all tested)

- **Windows.** At every moment at most one side may reveal, by the clock and the stage
  (`docs/reverse-flow.md`, Terms and the contract). Forward: the user claims once ready, or from
  `t0`, until `t1`; the maker cancels an unready swap until `t0` and refunds from `t1` for
  `LATE_WINDOW`, then the user may claim for good. Reverse: the maker claims from ready until `t1`;
  the user refunds while unready and from `t1`. `ready` only before `t0`; `open` and `openReverse`
  refuse a `t0` under `MIN_WINDOW` from landing, or a `t1` under it from `t0`.
- **Reveal judgements** are pure functions on one read, the swap at the newest block's hash
  (`Settlement::read`), in `evm/judge.rs`. A window opens by the block's time and ends by the
  later of it and the wall clock, less the margin. Before `t0` a claim needs a ready with
  `READY_DEPTH` (2) blocks above it; a reverse claim one with `REVERSE_READY_DEPTH` (4) above it.
  The audit's 97 cases replay against them (`crates/zecswap-chain/tests/windows-spec-vectors.json`,
  `windows_spec.rs`).
- **Contract identity.** Every client reads `MIN_WINDOW()` and `LATE_WINDOW()` and refuses the lock
  contract, whose six-word record alloy decodes silently under the four-word ABI. The relayer's
  `/v1/terms` and the maker's `/v1/info` say `contractKind: "windows"`, `minWindow` and
  `lateWindow`, and a forward accept answers `lateWindow`; the client refuses a relayer that names
  others.
- **Saved before sending:** the client's ready signature deadlines and its refund request. The
  client refuses a swap whose `t1` follows `t0` by under twice `CLAIM_MARGIN`.
- **Protect.** Reveals that go out privately go through Flashbots Protect: `evm::check_keeps_secrets`
  refuses any URL but Protect's default endpoint with `hint=hash` and its few safe parameters. A
  private reveal is never simulated (fixed `REVEAL_GAS`).
- **Fees.** On mainnet a reveal's fee cap is 5 gwei higher (`Network::reveal_headroom_wei`), so a
  surge can't strand it past its window; it still pays only the base fee and tip. Sends tip what
  the node suggests, at least `MIN_TIP_WEI`, under twice the next base fee (`evm::fees`); a public
  send unmined three blocks on goes again at its nonce (`land_publicly`). Every send but a private
  reveal is simulated before it goes, and `evm/canonical.rs` reads only canonical ABI calldata.
- **Relayed steps.** Every relayer `503` has `Retry-After`; the client posts again until the chain
  shows the step (`User::relayed`). A wallet may hand a forward claim over early
  (`POST /v1/claim/hold`) once its deposit is out, for the relayer to send the moment the window
  opens: the relayer then holds the share long before the claim, a trust the design accepts.
- **Tokens.** Where the maker asks for them, each accept spends a Privacy Pass token, handed back
  once the user pays in, so a device walks away from at most `tokens_per_day` swaps a day
  (`docs/tokens.md`).
- **Deposits.** A deposit address derives from the seed (BIP-44
  `m/44'/60'/(1000 + account)'/0/index`, testnet `1'`); the relayer moves its USDC into the
  wallet's Railgun balance on one EIP-3009 `receiveWithAuthorization`, from an account of its own,
  for a floor plus `feeBps`.
- **Sweeps** build with `OvkPolicy::Discard` (`Wallet::sweep`): both sides hold the joint account's
  viewing key. The app must sweep to a never-used address each time.
- **Payments proved ahead.** `Wallet::prove_payment` proves a payment unsigned and unstored, its
  notes held from every other payment (the backend's output locks) for some blocks;
  `send_proved` stores it, `release` gives it up, and one held too long is released, never sent.
  A holder's own held notes go first, so a payment lost in a restart is proved again from them.
- **Terms hash.** The contract stores only `hashTerms(terms)`; every call on a swap passes its
  `Terms` and reverts `WrongTerms` on a mismatch. Rust's one definition is
  `zecswap_core::Terms::hash`, and every relayer request carries `terms`.
- **Contract.** Swap ids and single-use maker shares are bound to the maker's address, and a
  reverse id (`reverseSwapId`) is never a forward one. `deposit` credits only what arrives. A
  payout shields to the note committed at open from a per-swap EIP-1167 vault whose returns can be
  rescued. `claim` is a pull payment, and the revealed share is stored (`Swap.secret`), so nothing
  reads event logs.
- **Networks.** `Network` (`testnet` or `mainnet`) fixes the Zcash network, chains, Railgun, USDC
  and lightwalletd. librustzcash's final NU7 releases schedule NU7 on both (mainnet 3,543,405,
  testnet 4,465,026). Nothing checks at run time that the crates schedule the next upgrade: move
  them before each one activates, or every Zcash transaction built after it carries the old branch
  and nodes refuse it. While an upgrade is scheduled, a Zcash transaction's expiry stops at the
  block before it (`expiry_before_upgrade`): one still unmined then reads as expired at activation
  and is built again under the new rules.

## Gotchas

- Sepolia repriced new state in October 2026: deploying ZecSwap takes ~28M gas and `open` ~360k.
  `forge script` simulates the old prices and runs out of gas: deploy with `cast send --create` at
  the node's estimate.
- anvil's fork errors quote the upstream URL, Alchemy key included: filter URLs out of anything
  printed from a fork (`sed -E 's#https?://[^ "]+#<url>#g'`).
- Alchemy's free tier caps `eth_getLogs` at 10 blocks, and load-balanced RPCs read a block behind.
  Railgun's SDK scans logs, so `balance.cjs` and `send.cjs` need the Infura URLs.
- `blake-hash` (BLAKE-512) panics on ARM, phones included; `zecswap-railgun` uses the portable
  `bloock-blake-rs`.
- Railgun accepts any ciphertext, and a note its receiver can't decrypt is lost: never change
  `zecswap-railgun`'s note code without `engine/check.cjs` passing.
- In Foundry tests, `vm.prank` applies to the next external call, views included: compute a note
  commitment before pranking.
- Zcash testnet blocks sometimes land every few seconds, so confirmation rules fire early.
- A wallet with no accounts would scan ~300k blocks of Ironwood's last shard; `Wallet::sync` skips
  until an account exists. A CLI wallet that never synced fails its deposit with "Must scan blocks
  first": run `status` first.
- `Wallet::sync` runs its own copy of `zcash_client_backend::sync::run` (`wallet/sync.rs`) that adds
  only new subtree roots: the library's re-puts all ~1,900 mainnet roots each run, seconds of
  hashing a sync. Diff the copy against the library's loop at every upgrade.
- The CLI resumes the `swap.json` in its data dir, and one in an older format stops it
  (`missing field t0`): rename a finished one `swap.json.finished-<date>`.
- Rust is pinned to 1.92, the Android SDK's Rust backend's. Hence alloy 1.8, as alloy 2 needs
  Rust 1.94.1; alloy-core (`alloy-primitives`, `alloy-sol-type-parser`) runs at 1.7. `cargo update`
  stays within the pin (resolver 3), but takes later `rsa` pre-releases as compatible, and they
  break `blind-rsa-signatures` 0.18: `zecswap-tokens` pins the one it builds against.
- `zcash_client_sqlite` needs `transparent-inputs` because `zcash_client_backend/pczt` turns it on.
- Don't reintroduce an orchard fork or SDK patch: signing goes through pczt's public Signer.
- Never sign EVM transactions through `ProviderBuilder::new()`'s default fillers: its nonce cache
  advances on failed sends. `evm::signing_provider` shows the safe stack.
- Async closures (`AsyncFnMut`) held in a future break `Send` for spawned tasks; poll with plain
  predicates instead.

## Conventions

- Commits: a single-line lowercase conventional subject with a scope (`fix(client): …`), no
  Claude/Anthropic attribution anywhere, and explicit paths when staging (no `git add -A`).
- Code: sparse comments (only non-obvious whys), no duplication, finish what you touch, keep
  clippy and fmt clean.

Maintainers keep deployment notes outside the repo, in the gitignored `docs/local/`:
@docs/local/agent-notes.md
