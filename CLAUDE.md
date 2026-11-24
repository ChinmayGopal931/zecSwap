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

Design docs: `docs/reverse-flow.md`, `docs/open-fee.md`, `docs/deposits.md`, `docs/railgun-sends.md`,
`docs/tokens.md`.

## Layout

| Path | Role |
|---|---|
| `crates/zecswap-core` | Pure crypto: shares, proofs of knowledge, the joint account (UFVK/UA), seed-derived keys (per-swap auth key, deposit addresses in `deposit.rs`), EIP-712 and EIP-3009 signing (`auth.rs`, which keeps the lock contract's digests for swaps still open there), PCZT signing with the combined key |
| `crates/zecswap-railgun` | Railgun keys and `0zk` addresses from the seed, the shield notes payouts go to, fee notes as their receiver reads them; `engine/` checks them against Railgun's engine and wallet SDK (Node: `check.cjs`, `balance.cjs`, `send.cjs`) |
| `crates/zecswap-chain` | Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the Ethereum client (`evm`: settlement and sends, `judge.rs` for claims, reverse refunds and readies, `deposits.rs`, `railgun.rs`, `funding`) |
| `crates/zecswap-api` | The maker's and relayer's wire types (serde), shared with wallets; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (quote, pay the open fee where one is asked, accept, verify on-chain, deposit, claim or refund), forward and reverse (`reverse.rs`), and a wallet's deposits (`Depositor`): the reference the apps mirror |
| `crates/zecswap-tokens` | Privacy Pass tokens (RFC 9577/9578) an accept spends: the wallet's side, and with `server` the check a service makes of them (`docs/tokens.md`) |
| `crates/zecswap-jni` | The Android wallet's bindings, `libzecswap.so` (`scripts/build-android.sh`) |
| `crates/zecswap-cli` | Testnet wallet and user CLI (`init`, `status`, `send`, `send-many`, `swap`, `swap --relayer`, `deposit`) |
| `crates/zecswap-sim` | The deterministic simulation (Testing, below): a simulated Ethereum node on an in-process EVM (`node.rs`, `rpc.rs`), HTTP link (`net.rs`), maker, relayer and Zcash ledger models, the wallet that drives the real `User` (`wallet.rs`), and the run with its plan and checks (`run.rs`); `reverse.rs` and `reverse/wallet.rs` the same for reverse swaps and `ReverseUser`; `regressions.txt` and `reverse-regressions.txt` hold seeds that once failed |
| `contracts` | Foundry: `src/ZecSwap.sol` (the windows contract), `test/deployed/ZecSwapLocks.sol` (the lock contract as deployed, kept as a record), `src/ShieldVault.sol` (per-swap payout vaults), `src/Deposits.sol`, `src/Pallas.sol`, `src/Token.sol`; tests: stateful invariants (`test/*.invariants.t.sol`), known answers (`test/Vectors.t.sol`, on Rust-made vectors in `test/vectors`), `test/fork` and `test/ffi`; `test/utils/TestToken.sol` is testnet's tUSD |
| `deployments/` | The mainnet contracts as deployed (`scripts/deploy-contract.sh` writes them) |
| `scripts/` | `build-android.sh` (the Android libraries), `deploy-contract.sh` (a mainnet contract) |

## Commands

```sh
cargo test --workspace                                  # the simulation (needs `forge build`), the audit's cases and the known answers
ZECSWAP_SIM_SEEDS=0..2000 cargo test -p zecswap-sim every_                # hunt both ways: ~0.4 s a seed each way; a range, or seeds by comma (every_seed or every_reverse_seed for one way)
ZECSWAP_SIM_SEEDS=42 ZECSWAP_SIM_TRACE=1 cargo test -p zecswap-sim story -- --exact --ignored --nocapture   # one seed's plan, verdict and every event (reverse_story for its reverse swap)
ZECSWAP_SIM_SEEDS=0..300 cargo test -p zecswap-sim survey -- --exact --ignored --nocapture   # one line a seed: what was planned, how it ended (reverse_survey likewise)
scripts/sim-mutants.py 0..150                           # breaks 25 client rules in turn, in place, and names the seeds that catch each; a run cut short is undone by the next; names after the range pick mutants
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

## Testing

No hand-written example tests (Conventions). These stand in for them:

- **The simulation** (`crates/zecswap-sim`). One seed is one whole forward swap: the real `User` and
  `Settlement` against a simulated Ethereum (the real contracts in an in-process EVM, behind the
  JSON-RPC a node speaks), a simulated maker, relayer and Zcash ledger, on one thread and a paused
  clock. The seed picks the plan (`Plan::from_seed`): the route (Railgun or an account), public or
  private reveals, the open fee and its cap, what the maker does (honest, never readies, vanishes,
  cancels early, keeps the fee, opens less, opens to another note, lies about its deadlines, opens
  far ahead), a relayer that skips payouts, and the faults: reorgs aimed at blocks that just mined,
  missed slots, delayed and lost transactions, fee swings, node reads that fail, stall or lag a
  block, HTTP answers that are busy, hang or get lost either way, a wallet killed and restarted,
  one that comes back minutes either side of `t0` or `t1`, one that stalls until seconds either
  side of the last moment it may deposit or claim, a skewed device clock. A fifth of seeds run in
  calm weather, where the swap must end as its maker's behaviour dictates; a tenth press the claim
  margin (a public claim seconds before `t1` on a chain that delays), and a tenth the ready depth
  (a two-block reorg behind every transaction).
- **Reverse swaps** (`reverse.rs`, its wallet in `reverse/wallet.rs`). The same seed run by
  `reverse::run` is one whole reverse swap: the real `ReverseUser` and `Settlement` against the
  same chain and ledger, a maker that quotes, pays its ZEC into the joint account and claims by
  `judge::reverse_claim`, and a relayer that funds the escrow with the two calls `funding_calls`
  returns, as Relay Adapt calldata but from tokens of its own, and sends `readyWithSig`, refunds
  and their payouts. The seed picks the plan (`reverse::Plan::from_seed`): the maker (honest, never
  pays in, pays in short, pays in late, never claims, claims late, claims at the last second
  regardless of its margin, quotes a lock of a day, lies in its quote about deadlines, refund note,
  user, amounts, deployment or share proof, reports another swap), the relayer (honest: holds a
  refund while a ready it sent may land and refuses a ready after a refund; skips payouts; sends the
  approval and not the open; hears refunds but answers busy for ten minutes; sends readies but
  loses its answers; careless: no holds, no refusals, no margin), private sends, the forward faults,
  a wallet back long after `t1`'s late window, and a user who wants its USDC back some time after
  funding or after confirming the ZEC, or stalls until seconds around the last moment to sign a
  ready or around `t1`. The wallet asks for a ready on every poll and lets the client refuse. A
  fifth of seeds run calm; seven twelfths are focused, a twelfth each: a refund asked just after
  confirming the ZEC, mostly on the relayer that loses its answers to readies; a refund asked before
  the ZEC arrives, on the busy relayer; the deposit spendable around the last moment to ready; a
  short deposit; a two-block reorg behind every transaction; a claim seconds before `t1`; a lying
  quote.
- **What it checks.** Every block: the windows the client judges by are the contract's, the two
  sides never may reveal at once, and the user's share is never public while the maker may still
  refund. At the end: no open fee over the cap, outside Orchard or built twice; no deposit to terms
  the user didn't agree or with under `MIN_TIME_TO_T0` left; a claimed swap carries the user's
  share and paid the user's note or account in full; a refunded one was swept back; a deposit is
  never left stuck once the weather has cleared. Reverse, every block: the same windows and
  exclusion, no transaction carrying either share just reverted, and the relayer never holds the
  user's share while the maker may claim. At the end: no funding asked for a lying quote, to another
  deployment, or locking the USDC more than a day; every ready signed with the whole deposit
  spendable, ending before `t0`, and none after a refund was asked; no refund asked while a signed
  ready may land, as the wallet saved it and as the relayer heard it, and no ready heard after a
  refund; the user's share public only once refunded and never while the maker may claim, the
  maker's only once claimed and never while the user may refund (but for the maker that ignores its
  margin); a claimed escrow left the wallet the whole deposit, a refunded one paid the refund note in
  full less the fee; nothing left stuck once the weather has cleared and `t1` has had time to pass.
  The same seed gives the same trace.
- **Working with it.** A failing seed prints its plan and last events: replay it with
  `ZECSWAP_SIM_SEEDS=<seed>`, fix the cause, and add the seed to `regressions.txt` (or
  `reverse-regressions.txt`), which every `cargo test` replays. A new behaviour is tested by a new
  fault, maker behaviour or check there, never by an example test. Faults stay within what the
  design assumes (reorgs two deep, a reveal landing inside `CLAIM_MARGIN`): past that a seed fails
  by design, not by a bug.
- **Its teeth.** `scripts/sim-mutants.py` removes a rule from the client and fails if no seed
  notices: forward, the ready depth, the claim margin, the deposit deadline, the terms check, the
  fee cap, the claim window's start, the maker's hour after `t1`; reverse, no ready after a refund,
  the whole deposit before a ready, a ready's life before `t0`, saving a ready's deadline and a
  refund before sending them, no refund while a signed ready may land or before `t1` when ready,
  the quote's deployment, amounts, user and note, deadlines and maker proof, the day's limit on a
  lock, the maker's ready depth and claim margin, both reverse windows, and judging a reverse
  escrow's reveals by its own windows. `regressions.txt` and `reverse-regressions.txt` also list
  seeds that caught each, so every `cargo test` would. A change to a plan or to a fault reshuffles
  what each seed does: run the script again and replace those seeds. The script touches every file
  it restores, so cargo rebuilds it: a mutant never stays compiled in after it is undone.
- **Its seams**, compiled only with the `sim` feature of chain and client: `evm::simulate_wall`
  (the wall clock, per task with `world::Skewed`), `Settlement::over` and
  `broadcast_privately_over` (an alloy transport in place of a URL), `MakerApi::over` and
  `RelayerApi::over` (a `Wire` in place of the network), and `ReverseUser::ready_with_spendable`
  (`ready` from its deposit check on, on a spendable balance the caller gives in place of the Zcash
  wallet's; `ready` runs the same code after its wallet sync). Waits measure with tokio's clock, so a
  paused runtime moves them. Not seeded: the OS randomness in the client's proofs, which no
  decision depends on. A whole-workspace build turns the feature on for every crate, inert until
  one of these is called; a `-p` build, as the scripts and `cargo run -p zecswap-cli` make, leaves
  it out.
- **Not simulated yet**, and so without tests beyond the known answers: deposits (`Depositor`),
  tokens, the Zcash wallet and lightwalletd client (so neither the wallet half of
  `ReverseUser::ready` nor `watch_deposit`), PCZT signing's refusals, the relayer-side request
  checks (`evm/funding.rs`, `evm/canonical.rs`, the policies in `evm/railgun.rs` and
  `evm/deposits.rs`), the Railgun cross-contract funding of an escrow and its proof (the relayer
  funds from tokens of its own), rescues, reverse status and token returns, relayers that serve
  another deployment, ask over the fee cap or are the maker, the CLI's own resume logic (the
  simulation's wallets mirror it) and the JNI bindings.
- **Open finding.** Reverse seed 182 fails (`the escrow is stuck at stage 1`), on a defect not yet
  fixed: `Settlement` raises the lowest nonce it signs privately at the first receipt it sees
  (`landed_privately`) and never lowers it, so a private send reorged out for good leaves every
  later one from that account a nonce too high, unminable until the process restarts. Add the seed
  to `reverse-regressions.txt` with the fix.
- **Known answers** from outside implementations: Railgun's engine (`zecswap-railgun`), ethers'
  deposit addresses, the Privacy Pass RFC's token, the contract's hashes, Railgun's SDK funding
  vector, the audit's 97 window cases (`windows_spec.rs`), librustzcash's NU7 heights and circuit,
  the Android port's vectors (`zecswap-jni/tests`), and in Foundry `test/Vectors.t.sol`.
- **The contract**: Foundry's stateful invariants (`test/*.invariants.t.sol`), the fork tests and
  the Rust-against-Solidity fuzzing (`test/ffi`).

## How it behaves

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
  private reveal is never simulated (fixed `REVEAL_GAS`); `Settlement::reveal_need` says what its
  account must hold for a node to take one.
- **Fees.** On mainnet a reveal's fee cap is 5 gwei higher (`Network::reveal_headroom_wei`), so a
  surge can't strand it past its window; it still pays only the base fee and tip. Sends tip what
  the node suggests, at least `MIN_TIP_WEI`, under twice the next base fee (`evm::fees`); a public
  send unmined three blocks on goes again at its nonce (`landed_publicly`). Every send but a private
  reveal is simulated before it goes, and `evm/canonical.rs` reads only canonical ABI calldata.
- **Send outcomes.** A contract call that fails says whether anything went out
  (`Error::nothing_sent`): `WouldRevert` where the node ran its simulation and it reverts, `Unsent`
  where no node took the transaction, `Late` for a reveal or a ready out of time. Once a node has
  it, the error names the transaction (`Reverted`, `Unconfirmed`, `Dropped`). A ready on a
  signature goes out only with its caller's margin left before its deadline and `t0`, judged
  again in its account's turn (`ready_with_sig`). The notes a Railgun send spends are asked about
  in one call (`spends_spent_notes`), and `balance_of_at` reads an inventory at a named block.
- **Relayed steps.** Every relayer `503` has `Retry-After`; the client posts again until the chain
  shows the step (`User::relayed`). A wallet may hand a forward claim over early
  (`POST /v1/claim/hold`) once its deposit is out, for the relayer to send the moment the window
  opens: the relayer then holds the share long before the claim, a trust the design accepts.
- **Open fee.** Where the maker asks one, a forward quote names a fee in ZEC and an address of
  its own (`docs/open-fee.md`): the wallet pays it before accepting, and the maker opens the swap
  only once it is mined. The client refuses a fee over its caller's cap (none, unless
  `with_max_open_fee`) or payable outside Orchard, pays only with `OPEN_FEE_MARGIN` of the quote
  left, and posts its accept again through `feeRequired` until the quote expires
  (`FeeNotHonoured`). The CLI saves the quote before it pays and the payment before it sends. The
  fee is the maker's whatever follows; the deposit still waits for the contract.
- **Tokens.** Where the maker asks for them, each accept spends a Privacy Pass token, handed back
  once the user pays in, so a device walks away from at most `tokens_per_day` swaps a day
  (`docs/tokens.md`).
- **Deposits.** A deposit address derives from the seed (BIP-44
  `m/44'/60'/(1000 + account)'/0/index`, testnet `1'`); the relayer moves its USDC into the
  wallet's Railgun balance on one EIP-3009 `receiveWithAuthorization`, from an account of its own,
  for a floor plus `feeBps`.
- **Sweeps** build with `OvkPolicy::Discard` (`Wallet::sweep`): both sides hold the joint account's
  viewing key. The app must sweep to a never-used address each time. A sweep spends every note
  of its account, and one that would pay the network over `SWEEP_FEE_CEILING` (64 notes' worth)
  is refused before its proof (`Error::SweepFee`); `Wallet::sweep_fee` says what one would pay
  with nothing proved.
- **Payments proved ahead.** `Wallet::prove_payment` proves a payment unsigned and unstored, its
  notes held from every other payment (the backend's output locks) for some blocks;
  `send_proved` stores it, `release` gives it up, and one held too long is released, never sent.
  One that fails to prove, sign or store gives its notes back at once. A holder's own held notes
  go first, so a payment lost in a restart is proved again from them.
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
- The CLI resumes the `swap.json` in its data dir, and one in another version's format stops it
  (`swap.json is another version's`): rename a finished one `swap.json.finished-<date>`.
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
- Code: no comments, no duplication, finish what you touch, keep clippy and fmt clean. The only
  comments are the CLI's help text (clap reads `///` in `zecswap-cli/src/main.rs`), SPDX lines, and
  those of the deployed contract sources (`contracts/src`, `test/deployed`, `TestToken.sol`), whose
  text the compiler hashes into the bytecode: change nothing there but the logic you mean to.
- Tests: no hand-written example tests. A behaviour goes into the simulation (Testing); a format
  shared with an outside implementation gets a known answer from it.

Maintainers keep deployment notes outside the repo, in the gitignored `docs/local/`:
@docs/local/agent-notes.md
