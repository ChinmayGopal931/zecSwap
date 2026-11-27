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
| `crates/zecswap-chain` | Zcash light wallet over lightwalletd (sync, joint accounts, pay, sweep) and the Ethereum client (`evm`: settlement and sends, `judge.rs` for claims, reverse refunds and readies, `deposits.rs`, `railgun.rs`, `funding`), with the alerts both raise (`alert.rs`) |
| `crates/zecswap-api` | The maker's and relayer's wire types (serde), shared with wallets; the Kotlin port's spec |
| `crates/zecswap-client` | The user side, step by step (quote, pay the open fee where one is asked, accept, verify on-chain, deposit, claim or refund), forward and reverse (`reverse.rs`), and a wallet's deposits (`Depositor`): the reference the apps mirror |
| `crates/zecswap-tokens` | Privacy Pass tokens (RFC 9577/9578) an accept spends: the wallet's side, and with `server` the check a service makes of them (`docs/tokens.md`) |
| `crates/zecswap-jni` | The Android wallet's bindings, `libzecswap.so` (`scripts/build-android.sh`) |
| `crates/zecswap-cli` | Testnet wallet and user CLI (`init`, `status`, `send`, `send-many`, `swap`, `swap --relayer`, `deposit`) |
| `crates/zecswap-sim` | The deterministic simulation (Testing, below): a simulated Ethereum node on an in-process EVM (`node.rs`, `rpc.rs`), HTTP link (`net.rs`), maker, relayer and Zcash ledger models, the wallet that drives the real `User` (`wallet.rs`), and the run with its plan and checks (`run.rs`); `reverse.rs` and `reverse/wallet.rs` the same for reverse swaps and `ReverseUser`; `deposits.rs` and `deposits/` (its wallet, its relayer and a paid-out swap) for a wallet's deposits, `Depositor` and rescues; `lightwallet.rs` and `lightwallet/` (a Zcash chain of real notes, its lightwalletd, the wallet that drives the real `zcash::Wallet`, and sweeps for the signer) for the Zcash wallet, the lightwalletd client and PCZT signing; `alerts.rs` for the senders' alerts and the operator who acts on them; `hunt.rs` for seeds run on every core and hunts over ranges of them, and `reach.rs` for the situations a seed reached; `regressions.txt`, `reverse-regressions.txt`, `deposit-regressions.txt` and `zcash-regressions.txt` hold seeds that once failed |
| `crates/zecswap-sim-evm` | The simulated node's EVM: one call or transaction run by revm on a block's state, in a crate of its own so that it builds optimised (Testing, Its seams) |
| `contracts` | Foundry: `src/ZecSwap.sol` (the windows contract), `test/deployed/ZecSwapLocks.sol` (the lock contract as deployed, kept as a record), `src/ShieldVault.sol` (per-swap payout vaults), `src/Deposits.sol`, `src/Pallas.sol`, `src/Token.sol`; tests: stateful invariants (`test/*.invariants.t.sol`), known answers (`test/Vectors.t.sol`, on Rust-made vectors in `test/vectors`), `test/fork` and `test/ffi`; `test/utils/TestToken.sol` is testnet's tUSD |
| `deployments/` | The mainnet contracts as deployed (`scripts/deploy-contract.sh` writes them) |
| `scripts/` | `build-android.sh` (the Android libraries), `deploy-contract.sh` (a mainnet contract) |

## Commands

```sh
cargo test --workspace                                  # the simulation (needs `forge build`), the audit's cases and the known answers
ZECSWAP_SIM_SEEDS=0..2000 cargo test -p zecswap-sim every_                # all four runs over a range, or seeds by comma, on half the cores (ZECSWAP_SIM_THREADS for another count): ~0.15 s of CPU a seed each, the Zcash wallet's ~3.5 s, most of it proofs (every_seed, every_reverse_seed, every_deposit_seed or every_zcash_seed for one)
ZECSWAP_SIM_SEEDS=0..20000 ZECSWAP_SIM_RUNS=forward,reverse,deposits,zcash=0..2000 cargo test -p zecswap-sim hunt -- --exact --ignored --nocapture   # a hunt (Testing, Hunts): failures by kind and what was reached, kept in docs/local/sim-hunts (ZECSWAP_SIM_HUNTS for another folder); the same command again goes on where it stopped
ZECSWAP_SIM_SEEDS=42 ZECSWAP_SIM_TRACE=1 cargo test -p zecswap-sim story -- --exact --ignored --nocapture   # one seed's plan, verdict and every event (reverse_story for its reverse swap, deposit_story for its deposits, zcash_story for its Zcash wallet)
ZECSWAP_SIM_SEEDS=0..300 cargo test -p zecswap-sim survey -- --exact --ignored --nocapture   # one line a seed: what was planned, how it ended (reverse_survey, deposit_survey and zcash_survey likewise); ZECSWAP_SIM_DIGEST=1 adds a digest of each seed's plan, verdict and whole trace
scripts/sim-mutants.py 0..150                           # breaks 89 client rules in turn, in place, and names the seeds that catch each (hours, as every mutant rebuilds the crates it touches; a run cut short is undone by the next); names after the range pick mutants
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
- **Deposits and rescues** (`deposits.rs`, its wallet, relayer and paid-out swap in `deposits/`).
  The same seed run by `deposits::run` is an hour of one wallet's deposit addresses: public tUSD
  arriving at them (at the address shown, at one used before, some addresses ahead, past the gap,
  twice within seconds, under the minimum and topped up, after a rescue), the real `Depositor`
  recovering from the seed, signing, posting and resuming shields and rescues against the real
  `Deposits.sol`, a wallet that works as `zecswap-cli deposit` does (each request saved before it is
  posted and posted again after a crash, one pending for each address), and a relayer that checks
  each request as `docs/deposits.md` says and sends it with `Settlement::send_deposit`. The seed
  picks the relayer (honest; asks a fee over the cap; names another Deposits contract, token or
  chain; takes no deposits; names a minimum of 1; shows terms that lag a new fee; sits on requests;
  answers and never sends; sends and loses its answers; sends with another fee, note or value,
  which reverts; answers with another deposit's transaction, an older one of the same address or
  one of another address), its terms (a max validity of some twenty minutes, a fee repriced under
  or over the cap), the wallet's cap, Multicall3 or none, how often Railgun sends a shielded note
  back to its vault (the simulation mints the note's value there), the forward faults, crashes and
  a skewed clock. A quarter of the unfocused seeds, and of those where every shield is sent back,
  also hold a swap whose payout, forward or reverse, Railgun sends back once or twice: its rescue
  is signed with `Domain::rescue` and posted by hand, as `zecswap-client` has no call for one. Three
  quarters of seeds are focused, a sixteenth each: a short validity on a relayer that sits; lagging
  terms after a new fee; the two wrong transactions; a gap closed late (addresses 3, 47, then 22);
  every shield sent back, half with the fee then repriced over the cap; the minimum; a second
  arrival seconds after the first, with a third of transactions lost; a fee over the cap; another
  deployment; a tampered send; a relayer that goes silent, with a third of transactions lost. Base
  fees ease back once the weather clears (`Node::ease_fees`, for this run only), but for one seed in
  seventeen (`fees_stay`): two public payments leave one account within a minute or so, two-block
  reorgs follow transactions, and fees stay where the weather left them, so a payment reorged out
  holds back the one behind it. A third of those make three payments at once under steady fees, so
  the queue behind a reorg drains a number at a time.
- **The Zcash wallet, lightwalletd and the signer** (`lightwallet.rs`; the chain in
  `lightwallet/ledger.rs`, lightwalletd in `server.rs`, the wallet in `wallet.rs`, sweeps for the
  signer in `signer.rs`). The same seed run by `lightwallet::run` is an hour or two of one
  `zcash::Wallet` on its sqlite file: the real wallet, through the real `Lightwalletd` client over
  an in-memory HTTP/2 connection, against a simulated Zcash chain of real notes (compact blocks with
  real note encryption, commitment trees whose earlier shards are made up, subtree roots, other
  users' notes) and a node that judges every broadcast as a node would: the consensus branch and
  bundle versions for its height, expiry, anchors, nullifiers, the ZIP 317 fee, proofs and
  signatures. The wallet runs errands as the CLI and a maker do: it receives into a joint account
  and sweeps it to a fresh address of its own or to another wallet, or watches and forgets it; pays
  from its own account; proves a payment ahead and then sends it, releases it, signs it with another
  key or holds it too long; watches fee addresses; broadcasts again every few polls while a
  transaction is unmined; and a second party hands the signer (`sign_pczt_bytes`) a sweep it built:
  honest, in each view a wallet SDK shows a signer, or paying another receiver, skimming, spending
  part, overpaying the network, unreadable by its receiver, with its value hidden, already signed,
  claiming a receiver it does not pay, or for another key. The seed picks the network, the era (well
  before NU7, across its activation, after it), the confirmations, the wallet's kind (an account of
  its own by `seed_account` or `inventory_account`, or joint accounts only), and the faults: reorgs
  up to three deep, after which a payment or transaction may wait or be lost and another holder of a
  joint key may spend what a sweep spent, blocks seconds apart, a lightwalletd that errors, hangs
  up, answers late, cuts a stream of blocks, stalls a call three ways (never answers, goes silent,
  trickles bytes), names a tip a block or two behind, answers a young wallet's step back with the
  tree state of another height, loses its answer to a broadcast, answers a
  repeat with an error or with none, or serves another network, and a wallet killed and reopened.
  Most wallets start with some history (a burst of blocks carrying other users' notes); one in
  eighty is fresh, one in eighty is served by two lightwalletds in turn, one of them two blocks
  behind, with a fresh wallet and no reorgs, and one in eighty watches a joint account whose
  payments are lost with their reorged block. Three quarters of seeds are focused, a
  sixteenth each: reorgs behind every transaction; a transaction built just before the upgrade and
  delayed past it; a first transaction lost, to expire and be built again; a payment proved ahead
  and the wallet killed while it holds it; 65 to 68 notes in a joint account; joint accounts one
  after another in a wallet with no other; a stalling lightwalletd; lost answers to broadcasts with
  a crash after building; each way a held payment ends; hostile sweeps for the signer; fee addresses
  under reorgs; restarts of an inventory or seed account. Some four seeds in ten prove, one to three
  times, at ~0.8 s a proof; no prover is faked. A seed's wallet file starts as a copy of one the
  wallet migrated from empty earlier in the process, once a network, so a seed's first open finds
  its schema in place as a later one does.
- **Alerts in the simulation** (`alerts.rs`). Every sender's alerts (How it behaves) reach one sink,
  which holds each against the chain as it is raised: the account's count, the transactions it
  names, whether they are mined or still in the pool, how many blocks a send has waited at that
  count, and that a stuck private sender is told of when its send is dropped (or, where that read
  failed, at the next one). Some alerts make the sink panic. A minute to ten later, for half
  of alerts one to two hours, an operator does by hand what the alert asks: restarts the sender
  (`Settlement::forget_sends`, what a new process would not remember), or sends the stuck number
  again at a higher fee; the run goes on at least forty minutes past that. A private transaction
  back from a reorg is dropped 25 blocks later, as Protect would. Nobody acts on the Zcash wallet's
  alert.
- **What it checks.** A panic in any task of a run, aborted ones included, fails its seed
  (`panics.rs`). Every block: the windows the client judges by are the contract's, the two
  sides never may reveal at once, and the user's share is never public while the maker may still
  refund. At the end: no open fee over the cap, outside Orchard or built twice; no deposit to terms
  the user didn't agree or with under `MIN_TIME_TO_T0` left; a claimed swap carries the user's share
  and paid the user's note or account in full; a refunded one was swept back; a deposit is never
  left stuck once the weather has cleared. Reverse, every block: the same windows and exclusion, no
  transaction carrying either share just reverted, and the relayer never holds the user's share
  while the maker may claim. At the end: no funding asked for a lying quote, to another deployment,
  or locking the USDC more than a day; every ready signed with the whole deposit spendable, ending
  before `t0`, and none after a refund was asked; no refund asked while a signed ready may land, as
  the wallet saved it and as the relayer heard it, and no ready heard after a refund; the user's
  share public only once refunded and never while the maker may claim, the maker's only once claimed
  and never while the user may refund (but for the maker that ignores its margin); a claimed escrow
  left the wallet the whole deposit, a refunded one paid the refund note in full less the fee;
  nothing left stuck once the weather has cleared and `t1` has had time to pass. Deposits, at the
  end, from the token's and the contracts' own events: nothing left a deposit address but on an
  authorization the wallet signed, for that value, fee and validity; every deposit and rescue that
  landed was to a note and for a fee the wallet signed together, and none landed twice; nothing left
  a vault but to Railgun or as that fee; every note shielded from a vault opens with the wallet's
  keys; no fee signed over the cap or leaving nothing, no deposit under the minimum, none for a
  relayer whose terms lie, none valid for longer than the relayer takes; every request the relayer
  heard was saved first and had time to land; every deposit the wallet believed shows in the
  contract's events for its address, and one to another note than it asked only where the relayer
  named a conflict; no rescue signed for a vault holding no more than its fee; a scan from the seed
  finds what the chain holds, and shows the first address never deposited from; nothing a relayer
  that delivers could shield is left at an address the scan reaches, in its vault or in the swap's
  once the weather has cleared; the swap's rescue nonce counts its rescues. In those three runs no
  alert is false, no public send goes out again for nine blocks behind an unmined one of its account
  without an alert, and a queue is gone eighty blocks after the operator acts (a private sender left
  stuck shows as a stuck swap). Zcash wallet, after
  every sync that read the true tip: each account's funds are what the chain holds for its key, in
  total and at its confirmation depth, never more to spend than is confirmed, and read alike alone
  and among all; a transaction is called mined, confirmed or expired exactly as the chain has it;
  fee addresses received what the chain shows. Along the way: an imported joint account is known by
  its key and no other's, found again by a second import, gone once forgotten; another seed is
  refused as the inventory's and a restart finds the same account; a sweep goes to an address never
  used; a sweep over `SWEEP_FEE_CEILING` is refused as `SweepFee`, at the fee quoted, its funds
  untouched; a payment proved ahead takes its notes out of the spendable balance, is proved again
  from them after a restart, cannot be sent once held too long, and gives them back when released or
  when signing fails; a broadcast the wallet calls accepted is held by the node and one it calls
  refused is not; no call stays open past its deadline; a refused rewind
  raises its alert, and only where a reorg took the block the wallet was born on. At the end: every
  transaction the wallet built was taken by the node at first sight, or refused only for having gone
  stale; none may outlive its consensus branch; each pays the ZIP 317 fee; a sweep spent all its
  account held, paid its destination all but the fee, and left nothing readable by the joint viewing
  key and no note behind; a payment paid its amount and returned the change; no errand was paid
  twice; nothing the signer signed pays its destination less than its intent or the network more, an
  honest sweep is never refused, and one proved is taken and mined; with time enough once the
  weather cleared, every errand ended as planned. The same seed gives the same trace, alone or with
  others running beside it on other threads.
- **Working with it.** A failing seed prints its plan and last events: replay it with
  `ZECSWAP_SIM_SEEDS=<seed>`, fix the cause, and add the seed to `regressions.txt` (or
  `reverse-regressions.txt`, `deposit-regressions.txt`, `zcash-regressions.txt`), which every
  `cargo test` replays. A new behaviour is tested by a new
  fault, maker behaviour or check there, never by an example test. Faults stay within what the
  design assumes (reorgs two deep, a reveal landing inside `CLAIM_MARGIN`): past that a seed fails
  by design, not by a bug.
- **Hunts** (`hunt.rs`; the test `hunt`). Seeds run as many at once as `ZECSWAP_SIM_THREADS` says,
  half the cores unless it is set, each on a thread and a paused clock of its own, and are told in
  seed order: `every_*`, the regressions, the replays and the surveys spread so too, and every seed
  `cargo test` runs takes one of those seats. The prover's pool gets as many threads again
  (`RAYON_NUM_THREADS` for another count), so at most twice that many are busy, and only while
  Zcash seeds prove. A hunt runs a range of seeds through the runs named in `ZECSWAP_SIM_RUNS` (all
  four by default; `zcash=0..2000` gives a run a range of its own) and keeps what it finds under
  `docs/local/sim-hunts`, or the folder `ZECSWAP_SIM_HUNTS` names, in a folder of the tree's: its
  commit and, where files are changed, a hash of the changes, as a seed means something for one
  tree only. There: `seeds.jsonl`, a line a seed with its digest (plan, verdict and whole trace),
  its failures and what it reached, which the same command reads to go on where a hunt stopped;
  `failures.txt`, each failing seed's survey line, plan, failures and replay command;
  `summary.txt`, what the hunt printed, and `reach.txt`. Failures are grouped by their text with
  numbers, hashes and the checkout's path taken out, and each way of failing is named by eight hex
  digits: a line `<name> <label>` in `labels.txt` of the hunts folder marks one as known, the rest
  show as NEW. Reach (`reach.rs`) counts, run by run, the seeds that met each situation worth
  meeting: every variant a plan picks, every way a run ends, each alert and what the operator did
  about it, reorgs by depth, a wallet killed before or after its deposit, and by their text the
  refusals the client and the wallet voiced. The summary names the situations no seed reached.
  Nothing a hunt finds belongs in a tracked file or on CI: a failing seed gives its finding away.
- **Its teeth.** `scripts/sim-mutants.py` removes a rule from the client and fails if no seed
  notices: forward, the ready depth, the claim margin, the deposit deadline, the terms check, the
  fee cap, the claim window's start, the maker's hour after `t1`; reverse, no ready after a refund,
  the whole deposit before a ready, a ready's life before `t0`, saving a ready's deadline and a
  refund before sending them, no refund while a signed ready may land or before `t1` when ready, the
  quote's deployment, amounts, user and note, deadlines and maker proof, the day's limit on a lock,
  the maker's ready depth and claim margin, both reverse windows, and judging a reverse escrow's
  reveals by its own windows; deposits, the terms' chain and deployment, the minimum, the fee cap, a
  fee that leaves nothing, the relayer's max validity, saving a request before posting it (the
  first, one signed again when stale, one signed again when refused, and a rescue), signing again
  when stale, posting again when a transaction never lands, believing a deposit only of this address
  and only to this note, a rescue's fee cap, no rescue of nothing, the scan's gap, the address
  shown, what counts as a used address, and the digest a swap's rescue signs; the Zcash wallet,
  syncing with no account or born above the tip, finding a joint account by its key and telling it
  from another, both pools' funds, a sweep's fee ceiling, its discarded outgoing key and its fresh
  address, a proved payment's hold, its own notes first, its lapse, its release and its giving back,
  the confirmation depth, expiry and its stop before an upgrade, receipts only once mined, the
  inventory's seed and the seed's account, lightwalletd's two deadlines, its network, the look-up
  behind a refused broadcast, an orphaned answer and a refusal's code, stepping a young wallet back
  to its first checkpoint, and only there, a second tip under the wallet's birthday, and the
  signer's receiver, fee cap, least amount, readable ciphertext and note commitment; the alerts, the
  private one and the queue's never raised, the private one with no send dropped or only at the next
  send, the queue's at once or while it drains, the wallet's on any reorg, and a sink's panic let
  through. Three more turn a refusal into a panic. The four regressions lists also name seeds that
  caught each, so every `cargo test` would. A change to a plan or to a fault reshuffles what each
  seed does: run the script again and replace those seeds. For a run with open findings the script
  first counts the seeds that fail unchanged and leaves them out. The script touches every file it
  restores, so cargo rebuilds it: a mutant never stays compiled in after it is undone.
- **Its seams**, compiled only with the `sim` feature of chain, client and railgun:
  `evm::simulate_wall` (the wall clock, per task with `world::Skewed`), `Settlement::over` and
  `broadcast_privately_over` (an alloy transport in place of a URL), `MakerApi::over` and
  `RelayerApi::over` (a `Wire` in place of the network), `ReverseUser::ready_with_spendable`
  (`ready` from its deposit check on, on a spendable balance the caller gives in place of the Zcash
  wallet's; `ready` runs the same code after its wallet sync), and `Settlement::forget_sends` (what
  a restart forgets: the lowest private nonce and what the alerts watch). Deposits need none to
  run, as `Depositor` takes a `Settlement` and a `RelayerApi`; with the feature a `Depositor` keeps
  each address it has derived, since every scan derives twenty from the seed again, which was three
  quarters of a deposit seed's time. Likewise `zecswap_railgun::Keys::from_seed` gives a thread the
  keys it derived last, when asked for the same seed and index again: each `User` a wallet builds
  derives them, a third of a forward seed's time. Nor does the Zcash wallet need a seam:
  `Lightwalletd` is a tonic client over a `DeadlineChannel`, which any `Channel` becomes, so one
  over an in-memory duplex stands in for a URL (the endpoint's own settings, `zcash::connect`'s,
  are not run). `zecswap-chain` builds at
  `opt-level = 3` under test: its proving code, generic over the randomness the wallet gives it, is
  compiled in that crate and runs seven times slower unoptimised. `zecswap-sim-evm` does for the
  same reason: revm's interpreter is generic over the state it runs on, so it is compiled where it
  is called, and `zecswap-sim` itself builds at `opt-level = 1` (at 3 it compiles for over ten
  minutes, which every mutant would pay). Waits measure with tokio's clock, so a paused runtime
  moves them. Not seeded: the OS randomness in the client's proofs, which no decision depends on. A
  whole-workspace build turns the feature on for every crate: the two that remember answer every
  caller with what the derivation gives, and the rest are inert until called; a `-p` build, as the
  scripts and `cargo run -p zecswap-cli` make, or one that names no crate (`default-members`)
  leaves it out.
- **Not simulated yet**, and so without tests beyond the known answers: tokens,
  `ReverseUser::ready`'s wallet half and `watch_deposit` as the client composes them (the wallet
  calls they make are simulated in the Zcash run, the reverse run still gives `ready` its spendable
  balance), the relayer-side request checks (`evm/funding.rs`, `evm/canonical.rs`, the policies in
  `evm/railgun.rs`; of `evm/deposits.rs`, the gas-price cap, the ETH check and `covers`, which the
  deposit relayer never trips), the Railgun cross-contract funding of an escrow and its proof (the
  relayer funds from tokens of its own), reverse status and token returns, swap relayers that serve
  another deployment, ask over the fee cap or are the maker, the CLI's own resume logic (the
  simulation's wallets mirror it) and the JNI bindings. Of deposits: the sanctions oracle, a deposit
  address with code, fees priced by gas (`feeExpiresAt`), deposits sent privately, a relayer's
  hour-long hold after a revert (five minutes here), and Railgun's screening itself. A swap payout's
  rescue has no caller in `zecswap-client`: the simulation's wallet is the only one, written from
  `docs/reverse-flow.md`. Of the Zcash wallet: Orchard-pool notes and their move to Ironwood,
  Sapling and transparent funds, `import_joint_view`, `Wallet::fee`, a sweep of exactly 64 notes (a
  64-action proof is half a minute), expiry after NU7 (120 blocks), scans over `SYNC_BATCH_SIZE`,
  `zcash::connect`'s endpoint settings and TLS, answers out of protocol (a tip past `u32`, a tree
  state of another fork than its blocks), the wall clock the backend stamps rows with, and subtree
  roots that matter: a wallet here scans every block from its birthday, so the roots `sync.rs`
  stores are never needed and its root bookkeeping has no mutant; nor has the rule that an empty
  frame does not reset a silent call's deadline, as the HTTP/2 library here delivers none, nor the
  ten blocks a reorg rewinds, as fewer still converge, nor `safe + 1 < at` beside the young wallet's
  step back, as no seed has a first checkpoint that high, its three steps a sync, as a seed that went
  past them would never end, nor which refusals alert, as no database error is simulated. The
  signer's refusals of transparent or Sapling parts, of a value balance that does not add up and of
  spends already signed are met only by sweeps other rules refuse too. The node model takes the
  sighash of a shielded-only transaction to be its txid digest, checks no ZIP 317 action count
  beyond the fee and no coinbase or turnstile rule.
- **Open findings** stay out of this repo: the maintainers keep them, with the seeds that fail, in
  `docs/local/open-findings.md`. Seeds outside the default ranges may fail on one of them.
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
- **Alerts.** What no code heals is raised as an `Alert` (`zecswap_chain::alert`): an error in the
  log, and a call to the sink a service gave (`Settlement::alerts_to`, `Wallet::alerts_to`), each
  time it is met; `Alert::key` names the fault, so a service can tell a person once. A sink must
  only enqueue: it runs on the sending path, with the sender's turn held (its panic is logged and
  goes no further). `PrivateNonceStuck`: private sends are signed above the chain's count because
  one seen mined was reorged out, and one sent since was dropped unmined. It is raised at that drop,
  on one more read of the account's count, or where that read fails once the next send is handed
  over; nothing the account sends privately can be mined until the service is restarted or a public
  send takes the number the chain expects, and one already handed to the private relay may still
  land for its window (`Private::window`, about 30 blocks). `SendQueued`: a public send has waited
  six blocks (`QUEUED_BLOCKS`) behind an earlier unmined transaction of its account, the wait
  starting again as the account's count advances; send that number again at a higher fee, replace
  it, or wait for fees to fall. `WalletRewindRefused`: the backend refused the Zcash wallet's step
  back from a reorg and later syncs may keep failing; nothing by hand is known to mend it, as a
  wallet rebuilt from its seed is born at the tip and sees nothing received before. Its key names
  the wallet's file.
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
  hashing a sync. Diff the copy against the library's loop at every upgrade. On a reorg it steps
  back ten blocks like the library's, and where the backend refuses (`RequestedRewindInvalid`: no
  scanned block that low has a checkpoint, as in a wallet's first ten blocks) to the first
  checkpoint, the block before its birthday, by `truncate_to_chain_state`: `truncate_to_height`
  refuses the very `safe_rewind_height` its error names, as that block was never scanned. It steps
  back so at most three times a sync, and only on a tree state of the height it asked for. A tip
  under the wallet's birthday, at either read of it, leaves nothing to sync (the backend's
  `update_chain_tip` panics).
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
