# Private sends and withdrawals through the relayer

A wallet sends privately from its Railgun balance, or withdraws from it to any Ethereum address,
without holding ETH. It proves the Railgun transaction itself, with a fee note to the relayer's
own 0zk address as the first output, and posts it to the relayer, which checks that note and pays
the gas, as Railgun's public broadcasters do. The phone's keys never leave it, no public account
of the phone's appears on the chain, and the relayer can delay a send but not change it: the proof
binds every output.

A relayer sends them only where its terms say so (below), on whichever chain its settlement
contract is: the chain ID and Railgun's proxy come from that contract. It sends only
plain `transact` calls to the proxy: no Relay Adapt, so no base-token unshields and no
cross-contract calls.

## Terms

`GET /v1/terms` includes `railgunSends` when the relayer sends them. When it is absent, the
relayer does not; don't fall back to a phone-funded transaction.

```json
{
  "relayer": "0x…",
  "chainId": 11155111,
  "contract": "0x…",
  "contractKind": "windows",
  "minWindow": 1800,
  "lateWindow": 3600,
  "fee": "20000",
  "railgunSends": {
    "railgunAddress": "0zk1…",
    "railgunProxy": "0xecfcf3b4ec647c4ca6d49108b311b7a7c9543fea",
    "token": "0x…",
    "fee": "500000",
    "feePerUnitGas": "2729606000",
    "feeExpiresAt": 1791504000,
    "maxGasLimit": 3000000,
    "maxGasPriceWei": "50000000000",
    "maxCalldataBytes": 65536
  }
}
```

| Field | Meaning |
| --- | --- |
| `railgunAddress` | The relayer's own 0zk address: the broadcaster fee recipient. |
| `railgunProxy` | Railgun's proxy, the transaction's `to`. Pin it, with `chainId`, against the SDK's own `NETWORK_CONFIG` for the network before proving. |
| `token` | The token the fee is paid in. |
| `fee` | Base units of `token` the fee note must carry at least (decimal string): the floor. |
| `feePerUnitGas` | Where the relayer prices gas, the rate its fee is held to: base units of `token` per 10^18 wei of gas cost, its margin included (decimal string), as Railgun's public broadcasters quote it. Absent while the relayer can't price gas, or where it prices none: then the fee is `fee` alone. |
| `feeExpiresAt` | With `feePerUnitGas`: until when (Unix seconds) a proof whose fee was worked out at this rate is held to it rather than a later one. Ten minutes from the terms. |
| `maxGasLimit` | The most gas the relayer pays for one send. |
| `maxGasPriceWei` | The highest gas price the relayer pays (decimal string), so the highest `overallBatchMinGasPrice` a proof may set. |
| `maxCalldataBytes` | The largest calldata it takes. |

The top-level `fee` is the swap payout fee and does not apply here.

## Proving

With `@railgun-community/wallet` 11.1.0 (engine 9.8.0), `TXIDVersion.V2_PoseidonMerkle`:

- A private send: `generateTransferProof`, then `populateProvedTransfer`. A withdrawal:
  `generateUnshieldProof`, then `populateProvedUnshield`, to the Ethereum address it pays.
- `sendWithPublicWallet = false`.
- The fee: with `feePerUnitGas`, estimate the send as the SDK does for a broadcaster,
  `gasEstimateForUnprovenTransfer` (or `gasEstimateForUnprovenUnshield`) with `feeTokenDetails =
  { tokenAddress: token, feePerUnitGas }` and `sendWithPublicWallet = false`, then
  `calculateBroadcasterFeeERC20Amount(feeTokenDetails, gasDetails)` with that estimate and the
  proof's gas price; pay that, or `fee` if it is more. Without `feePerUnitGas`, pay `fee`.
  `crates/zecswap-railgun/engine/send.cjs` (`priced`) does exactly this.
- `broadcasterFeeERC20AmountRecipient = { tokenAddress: token, amount: <that fee>,
  recipientAddress: railgunAddress }`. The SDK makes it the first output of the first transaction.
- `overallBatchMinGasPrice` no higher than `maxGasPriceWei`; the network's gas price when proving
  is a good choice. The relayer pays the higher of the network's price and this one.
- `populateProved*` takes gas details it only copies into the populated transaction; the relayer
  ignores them. Use `getEVMGasTypeForTransaction(network, false)` (Type 1 on Ethereum) with
  `gasPrice = overallBatchMinGasPrice`.
- No Relay Adapt: not `generateUnshieldBaseTokenProof`, not cross-contract calls.

Post the populated transaction's `to`, `data` and `value` as they are, with the network's chain ID.

## Request

`POST /v1/railgun/transact`, at most 132 KiB:

```json
{
  "chainId": 11155111,
  "to": "0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea",
  "data": "0xd8ae136a…",
  "value": "0"
}
```

Send these four fields and no others: the relayer refuses any other field with `422`. Persist
these exact bytes before posting, and keep the notes they spend locked until an answer below
releases them. Post the same bytes again after any answer that says to.

## Answers

| Status | `code` | What it means | What the wallet does |
| --- | --- | --- | --- |
| `200` | | `{"transactions": ["0x…"]}`: submitted, not yet mined. Posting the same bytes again, before or after it mines, answers with the same hash and sends nothing new. | Watch the transaction, then settle from the chain. |
| `400` | `rejected` | Refused: nothing from this proof was sent, and nothing will be. | Drop the send and free its notes. |
| `400`, `415`, `422` | `invalidRequest` | The body is not JSON (`400`), not sent as `application/json` (`415`), or not this request (`422`): a field missing, mistyped, or not one of the four. Nothing was sent. | As `rejected`. |
| `413` | `invalidRequest`, or none from the gateway | The body is over 132 KiB. Nothing was sent. | As `rejected`. |
| `404`, `405` | `notFound`, `methodNotAllowed` | This relayer has no such route. Nothing was sent. | As `rejected`. |
| `409` | `alreadySpent` | A note it spends is spent on chain, or a transaction the relayer already sent spends it, pending or mined: this proof or another of the same notes. `{"code": "alreadySpent", "error": "…", "transactions": ["0x…"]}` names the relayer's own transactions that spend them; it is empty when the notes went in one the relayer didn't send or no longer remembers. | Settle from the chain; don't free the notes. |
| `503` | `unavailable` | Not now, nothing sent, with `Retry-After` in seconds: an earlier send of these notes has no known outcome yet (15), one reverted less than an hour ago (the seconds left of that hour), the relayer can't price gas right now (60), or the gateway is busy (5). | Post the same bytes again after `Retry-After`. |
| `500` | `internal` | The relayer could not read the chain or record the send. Nothing is known. | Post the same bytes again later. |

A transport failure, a timeout or any other answer: post the same bytes again later. The relayer
records each transaction before broadcasting it, so a repeat never sends a second one; once the
first is known it answers `200` with its hash, or `409` naming it to another proof of the same
notes. A
recorded transaction no node knows is broadcast again after a minute if its nonce is still free,
and forgotten once its nonce went to another transaction mined twelve blocks deep, after which the
same bytes are checked and sent afresh. `200` means submitted: the transaction can still revert,
and the notes are spent only once their nullifiers are on chain. The same bytes keep naming a
send that reverted; to try again with notes it left unspent, prove again. The relayer sends no
proof of those notes until an hour after the send that reverted, and answers `503` until then,
so a wallet can't make it pay for failed sends of the same notes over and over.

The `rejected` reasons, in the `error` field, for the wallet's logs:

- `this relayer sends no Railgun transactions`, `wrong chain`
- `wrong destination or ETH value`, `the calldata is too large`, `not a Railgun transact call`,
  `noncanonical transact calldata`, `the calldata holds more than the relayer takes`
- `expected at least one Railgun transaction`, `each Railgun transaction must spend notes`, `a
  proof is for another chain`, `a proof is bound to an adapt contract`, `redirected unshields are
  not supported`, `a withdrawal must pay out the relayer's token`
- `a proof's minimum gas price is above the relayer's cap`, `the transaction does not pay the
  relayer's fee` (below `fee`, or, where it prices gas, below what the gas costs at the honored
  rate: fetch the terms again and prove again)
- `the network's gas price is above the relayer's cap`, `the transaction needs more gas than the
  relayer's cap`, `the relayer is low on ETH`, `the transaction fails in simulation`

Errors never carry calldata, ciphertexts or the chain node's own messages.

## What the relayer takes

The canonical ABI encoding of Railgun's V2.1 `transact(Transaction[])`, to the proxy, with no
ETH: at most 16 Railgun transactions, each on this chain, spending 1 to 13 notes into at most
13 outputs, with an annotation of at most 62 bytes and a memo of at most 1 KiB, bound to no
adapt contract, and unshielding, if at all, normally and only the relayer's token.
The first output of each Railgun transaction pays the relayer: notes to `railgunAddress` in its
token, together at least the fee. The relayer simulates the call before it sends it, which runs
Railgun's own checks of the proofs, and sends within its gas caps, with no retries.

## Screening

Where Railgun screens (POI), as on Sepolia and Ethereum, a send's outputs, the change, the
recipient's note and the relayer's fee, are spendable only once screening proofs of the send
reach Railgun's screening nodes. The sender's wallet proves its landed send itself after a
balance scan, once its tree of Railgun transaction IDs holds the send (`generatePOIsForWallet`
runs this on demand). The SDK also builds proofs before a send
(`preTransactionPOIsPerTxidLeafPerList`), which public broadcasters receive with the transaction
to check that its inputs were screened; this relayer neither takes nor checks them. They can't
screen the outputs: they are built on a stand-in transaction-ID tree and placeholder note
positions (`createDummyMerkleProof`, `getGlobalTreePositionPreTransactionPOIProof` in Railgun's
engine), so they name no note that lands, and forwarding them frees nothing. A landed note can
be screened on its own (`ppoi_submit_single_commitment_proofs`), but only with its key and
position: a broadcaster has those for its own fee note alone, and with them the screening node
can work out the note's amount against its commitment.

On Sepolia the wallet's own proofs are closed as of 2026-10-08: Railgun's indexer breaks its
chain of transaction verification hashes at index 4188 (block 11816741), so the SDK stops
syncing transaction IDs there and never proves a later send. Live, a relayed send's change stayed
`MissingExternalPOI` for 40 minutes while the screening nodes had validated the send. Until
Railgun mends the indexer, or a wallet repairs the list it syncs from (Veylor's plan, 2026-10-21),
a relayed send's outputs on Sepolia stay unspendable; spend other notes meanwhile. No relayer can
free them. On Ethereum the indexer's chain is whole (140,430 transactions on 2026-10-08) and the
screening node keeps up, so nothing blocks the wallet's own proofs there; a relayed send's outputs
clear once the wallet has proved it.
