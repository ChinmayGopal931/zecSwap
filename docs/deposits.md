# Deposits: public USDC into a wallet's private balance

A wallet that holds no ETH takes USDC from an exchange or any other wallet at a deposit address
of its own, derived from its seed, and turns it into private USD: it signs one EIP-3009
`receiveWithAuthorization` of the address's USDC to the Deposits contract, and the relayer sends
it. The contract takes the USDC into the address's own vault, pays the relayer the fee the
authorization names and shields the rest into the wallet's own Railgun balance, to a note the
wallet built. No ETH ever sits at the address, and the relayer can delay a deposit but not
change it: the authorization's nonce commits to the note, the fee and the relayer, and only that
relayer can land it.

A relayer takes them only where its terms say so (below). Buying ZEC then spends the private
balance through the reverse route ([reverse-flow.md](reverse-flow.md)).

## Two ways money comes in

- **Public USDC** goes to a deposit address, then through the Deposits contract, as below. It
  pays the relayer's fee and Railgun's shield fee (0.25%).
- **USDC already in Railgun** (another 0zk wallet) is sent straight to the wallet's 0zk address as
  an ordinary private transfer: no contract, no fee to the relayer and none to Railgun.

Never unshield 0zk USDC to a deposit address to shield it again: Railgun charges 0.25% each way,
and the amount becomes public.

## Deposit addresses

Each is an ordinary Ethereum account on the standard BIP-44 path, from the wallet's BIP-39 seed:

| Network | Path |
| --- | --- |
| mainnet | `m/44'/60'/(1000 + account)'/0/index` |
| testnet | `m/44'/1'/(1000 + account)'/0/index` |

`account` is the wallet's ZIP 32 account and `index` the deposit address's. Account 1000 and up
is never opened by default (MetaMask-style wallets use `m/44'/60'/0'/0/i`, Ledger Live
`m/44'/60'/x'/0/0`), so these addresses never mix with a user's own, yet any wallet that takes a
derivation path finds them. Testnet's coin type keeps testnet and mainnet addresses unlinked.
`zecswap_core::derive_deposit_keys(seed, network, account, index)` derives one;
`crates/zecswap-core/tests/deposit-vectors.json` holds addresses ethers derived from known seeds,
which the Rust tests and Foundry's `cast` agree with.

- **Never delegate a deposit address (EIP-7702) or use it for anything else.** The relayer
  refuses an address with code, and USDC would check a delegated address's signatures as a
  contract's.
- **Rotate.** Show the lowest index never shielded from: the one whose `deposited(address)` is
  false. An address holding less than `minValue` stays the one shown; once it is shielded, show the
  next. Old addresses keep working: whatever arrives at one later is shielded again, each signing
  to a note of its own.
- **Recover from the seed** by scanning 20 addresses at a time until 20 in a row are unused,
  where used means holding USDC or `deposited` true. One Multicall3 `aggregate3` per window reads
  both for all 20: the token's `balanceOf(address)` and the Deposits contract's
  `deposited(address)` (`Settlement::deposit_addresses`). Shield every used address holding at
  least `minValue`.

The relayer never watches deposit addresses: the wallet does, and signs once one holds enough.

## Pin the deployment

Pin per network the chain, the Deposits contract and its token, and check the terms against
them; never take them from the relayer. The mainnet deployment is recorded in
`deployments/mainnet-deposits.json`.

## Terms

`GET /v1/terms` includes `deposits` when the relayer takes them; when it is absent it doesn't.

```json
{
  "relayer": "0x…",
  "chainId": 11155111,
  "contract": "0x…",
  "contractKind": "windows",
  "minWindow": 1800,
  "lateWindow": 3600,
  "fee": "20000",
  "deposits": {
    "contract": "0x…",
    "relayer": "0x…",
    "token": "0x…",
    "fee": "100000",
    "feeBps": 10,
    "feeExpiresAt": 1791504000,
    "minValue": "2000000",
    "maxValidity": 86400,
    "maxGasPriceWei": "50000000000"
  }
}
```

| Field | Meaning |
| --- | --- |
| `contract` | The Deposits contract: every authorization's `to`. |
| `relayer` | The account the relayer sends deposits from. The nonce commits to it, and the fee goes there. It is not the top-level `relayer`. |
| `token` | The token deposits are in: USDC on mainnet, tUSD on testnet. |
| `fee` | Token base units of the fee before its percentage (decimal string): the floor, or where the relayer prices gas, a first deposit's gas at the gas price and ETH price now, if more. A rescue pays this alone. |
| `feeBps` | The fee's part of the deposit, in basis points. |
| `feeExpiresAt` | Where gas prices the fee: until when (Unix seconds) an authorization signed with this `fee` is taken though gas rose since. Ten minutes from the terms. Absent where the relayer prices no gas. |
| `minValue` | The least a deposit moves, in token base units (decimal string). |
| `maxValidity` | The latest an authorization's `validBefore` may be, in seconds from now. |
| `maxGasPriceWei` | The highest gas price the relayer pays (decimal string): above it deposits wait. |

A deposit of `value` pays `fee + ⌈value × feeBps / 10000⌉`
(`zecswap_api::relayer::deposit_fee`). The top-level `fee` is the swap payout fee and does not
apply here.

## Signing a deposit

1. Wait until the address holds at least `minValue`, and read the terms.
2. Work out the fee. **Cap what the wallet signs**, and since the fee has a percentage, the cap must
   grow with the deposit: a fixed part and a percentage of its own (the reference client's
   `FeeCap`). Refuse a fee over it, or not under the value.
3. **Show the fee and Railgun's 0.25% before signing.** The note receives
   `shielded − ⌊shielded × shieldFee / 10000⌋`, where `shielded = value − fee` and `shieldFee()`
   is Railgun's (25 today).
4. Build the note: `zecswap_railgun::Keys::note(&deposit_keys.shield_entropy(validBefore))`, for
   the wallet's own Railgun wallet (on Android, `AtomicSwap.shieldNote`, which libzecswap builds
   from both seeds and hands back with its commitment). The authorization's own `validBefore`
   gives each signing a note of its own, so no two deposits of one address share a note or a
   nonce, and the persisted request rebuilds it.
5. **Build the token's domain from the token**: read its `name()` and `version()`, and check
   `TokenDomain::separator()` against its `DOMAIN_SEPARATOR()`. Never hardcode either: USDC's is
   `"USD Coin"`, `"2"`, testnet's tUSD's `"ZecSwap Test USD"`, `"2"`.
6. The nonce:
   `DepositsDomain { chain_id, contract }.deposit_nonce(&note.words(), fee, &terms.relayer)`,
   `keccak256(abi.encode(DEPOSIT_TYPEHASH, chainId, contract, npk, encryptedBundle, shieldKey,
   fee, relayer))`, as the contract's `depositNonce(npk, ciphertext, fee, relayer)` computes it.
7. Sign EIP-3009's `ReceiveWithAuthorization(from, to, value, validAfter, validBefore, nonce)`
   with the deposit key (`TokenDomain::receive_with_authorization`, `DepositKey::sign`): `from`
   the deposit address, `to` the Deposits contract, `value` the address's whole balance,
   `validAfter` 0 and `validBefore` an hour from now, or `maxValidity` less a minute from now if
   that is sooner. The signature is `r ‖ s ‖ v`, with a low `s` and `v` 27 or 28.
8. **Persist the exact body**, then post it.

`zecswap_client::Depositor::sign_shield` does all of this.

## Request

`POST /v1/deposits/shield`, at most 16 KiB:

```json
{
  "chainId": 11155111,
  "contract": "0x…",
  "from": "0x…",
  "value": "25000000",
  "validAfter": 0,
  "validBefore": 1791507600,
  "note": {
    "npk": "0x…",
    "encryptedBundle": ["0x…", "0x…", "0x…"],
    "shieldKey": "0x…"
  },
  "fee": "125000",
  "signature": "0x…"
}
```

Send these fields and no others: the relayer refuses any other with `422`. Post the same bytes
again after any answer that says to.

## Answers

| Status | `code` | What it means | What the wallet does |
| --- | --- | --- | --- |
| `200` | | `{"transactions": ["0x…"]}`: sent, not yet mined. The same request posted again, before or after it mines, answers with the same hash and sends nothing new while it can still land. | Watch the transaction. Once it mines, believe it only if the pinned contract's `DepositShielded` in it names this address and this request's note. If it hasn't mined within a minute, post the same bytes again: the relayer names it again while it can still land, and sends the deposit anew once it can't, as when Flashbots dropped it (`zecswap-client`'s `REPOST`). |
| `400` | `rejected` | Refused: nothing was sent, and nothing will be. | If the fee or validity is the problem, fetch the terms and sign again; otherwise drop it. |
| `400`, `415`, `422` | `invalidRequest` | Not JSON (`400`), not sent as `application/json` (`415`), or not this request (`422`): a field missing, mistyped, or not one of these. Nothing was sent. | As `rejected`. |
| `413` | none, from the gateway | The body is over 16 KiB. Nothing was sent. | As `rejected`. |
| `409` | `conflict` | The authorization is used, or another deposit of this address is on its way: `{"code": "conflict", "error": "…", "transactions": ["0x…"]}` names the relayer's own that do it, empty when the relayer doesn't know one. | Follow the transaction named, believing it once the pinned contract's `DepositShielded` in it names this address; with none, read the chain. |
| `503` | `unavailable` | Not now, nothing sent, with `Retry-After` in seconds: the gateway is busy (5), gas can't be priced (60), the network's gas price is above `maxGasPriceWei` (300), the relayer is low on ETH (120), as many deposits are on their way as it sends at once (30), its private RPC refused the deposit (30), a deposit of this address reverted within the hour (the seconds left of it), or an earlier deposit of this address has no known outcome (15). | With `Retry-After` of 30 seconds or less, post the same bytes again, for up to 90 seconds in all (`zecswap-client`'s `BUSY_WAIT`, `BUSY_PATIENCE`); longer, show the deposit as waiting and post again with backoff. |
| `500` | `internal` | The relayer could not read the chain or record the deposit. Nothing is known. | Post the same bytes again later. |

Any other answer, a timeout or a transport failure: post the same bytes again later. The relayer
records each deposit before broadcasting it, so a repeat never sends twice. In every case, sign
again before `validBefore` is under 15 minutes away or the fee's honor window (`feeExpiresAt`)
runs out: the relayer takes no authorization with less than 15 minutes left to land.

The `rejected` reasons, in `error`, for the wallet's logs:

- `this relayer takes no deposits`, `wrong chain or Deposits contract`
- `a deposit moves at least …`, `a deposit moves at most what a Railgun note holds`, `the fee
  leaves nothing to shield`, `the authorization's validAfter is not two minutes past: sign 0`, `the
  authorization expires too soon to land: sign it again`, `the authorization is valid for longer
  than the relayer takes`, and for a rescue `the rescue expires too soon to land: sign it again`,
  `the rescue is valid for longer than the relayer takes`
- `not signed by the deposit address for this note, fee and relayer`
- `a fee of … is less than the relayer's …: fetch the terms again and sign again`
- `the deposit address has code: deposits are from plain accounts`, `the deposit address holds
  less than it authorizes`, `the deposit address is sanctioned`, `the rescue's nonce is not its
  vault's next`, `the address's vault holds nothing to rescue beyond the fee`
- `the transaction fails in simulation`, `the transaction needs more gas than the relayer's cap`,
  `the deposit's fee does not cover its gas`

Errors never carry signatures, notes or the chain node's own messages.

## Rescue

Railgun's wallets let a note its screening refuses go back only to the address that shielded it:
here the address's vault, `vaultOf(from)`. A rescue shields what came back again, to a note the
depositor signs for, less the fee for the relayer that sends it.

- Its note: `zecswap_railgun::Keys::note(&deposit_keys.rescue_entropy(nonce))` (on Android,
  `AtomicSwap.rescueNote`).
- The depositor signs `Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)`
  under the Deposits contract's EIP-712 domain (`"ZecSwap Deposits"`, `"1"`, the chain, the
  contract) with the deposit key (`DepositsDomain::rescue`): `note` is
  `keccak256(abi.encode(npk, ciphertext))` (`ShieldNote::commitment`), `relayer` the terms'
  `relayer`, `fee` the terms' `fee` alone, `nonce` the contract's `rescueNonces(from)`, and
  `deadline` at least 15 minutes and at most `maxValidity` away.
- `POST /v1/deposits/rescue`, with the same answers:

```json
{
  "chainId": 11155111,
  "contract": "0x…",
  "from": "0x…",
  "note": { "npk": "0x…", "encryptedBundle": ["0x…", "0x…", "0x…"], "shieldKey": "0x…" },
  "fee": "100000",
  "nonce": 0,
  "deadline": 1791507600,
  "signature": "0x…"
}
```

The relayer refuses a rescue whose nonce is not the vault's next, or whose vault holds no more
than the fee. A rescue that fails on chain consumes nothing.

## What to show

- **Received $X**: the address holds at least `minValue`.
- **Making it private…**: the deposit is posted, until it mines.
- **Private, available in about an hour**: it mined. Railgun's screening clears a shield before it
  can be spent: within minutes on Sepolia, about an hour on Ethereum.
- **Waiting for the network; your money is safe at your deposit address**: the relayer asked to
  wait longer than 30 seconds, or could not be reached.

## The contract

`contracts/src/Deposits.sol`: no owner, pause or upgrade.

| | |
| --- | --- |
| `shield(from, value, validAfter, validBefore, npk, ciphertext, fee, v, r, s)` | Takes `value` on `from`'s authorization, moves it into `from`'s vault, pays `fee` to the caller and shields the vault's whole balance less it. Emits `DepositShielded(from, value, fee, relayer, note)`, `value` what it shielded before Railgun's fee. |
| `rescue(from, npk, ciphertext, fee, nonce, deadline, signature)` | Shields the vault's whole balance again, less `fee`, on `from`'s signature. Emits `Rescued`. |
| `depositNonce(npk, ciphertext, fee, relayer)` | The nonce an authorization must carry. |
| `deposited(from)` | Whether `from`'s vault is there: it has deposited. |
| `vaultOf(from)`, `rescueNonces(from)`, `noteCommitment(npk, ciphertext)` | |
| `TOKEN()`, `RAILGUN()`, `SANCTIONS()` | Its token, Railgun's proxy, and on mainnet Chainalysis' sanctions oracle (`0x40C57923924B5c5c5455c48D93317139ADDaC8fb`). |

It refuses a sanctioned `from`, a fee of the whole value or more, and more than a Railgun note
holds. On a fork of Ethereum mainnet a first deposit from an address, which makes its vault,
burned 918,390 gas in execution, a later one 851,862 and a rescue 824,404 (2026-10-18,
`test/fork/DepositsFork.t.sol`).

## The reference client

`zecswap_client::Depositor` plays the wallet: `sign_shield`, `post_shield` (the answers above,
posting the same bytes again while a transaction it was given hasn't mined in a minute, signing
again once when a fee or validity went stale, and believing a transaction only once the contract's
event in it shows the address's deposit to the wallet's note), `shield`,
`sign_rescue`, `rescue`, `recover` and `showing`. `zecswap-cli deposit` runs it from a test wallet's seed, saving each
signed request before posting it.
