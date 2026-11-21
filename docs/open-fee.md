# The open fee: shielded ZEC → Railgun USDC

Opening a forward swap costs its maker gas before the user has paid anything: the transaction
that locks the USDC and, if no deposit follows, the one that takes it back. Where a maker asks for
it, the wallet pays that gas ahead, in ZEC, straight to the maker. It is the one amount in a swap
that rests on trust in the maker, and it is small. The deposit still moves only once the contract
shows the USDC locked. Reverse swaps have no such fee: there the user funds first.

## The flow

1. Take a quote as before, `POST /v1/quote`. Where the maker asks the fee, the quote carries
   `openFee`: `{ "zat": 41234, "address": "u1…" }`, zatoshis to pay and the address to pay them
   to, this quote's alone. A quote without it is accepted as before, with nothing paid.
2. Check the quote as before (the deployment, the maker's proof), then the fee
   ([below](#what-the-wallet-checks)). Show the user the fee as money that does not come back.
3. Persist the swap's index and the quote. Build one shielded payment of `zat` to `address`,
   with an empty memo, persist its transaction id, and only then send it. A reply-to address,
   or anything else in a memo, would tell the maker which wallet paid.
4. Wait until the wallet's own sync shows the payment mined. While no block has it and the quote
   has its 60 seconds left, send the same payment again every half minute: a node can take a
   payment and drop it, and the quote ends meanwhile.
5. Accept, `POST /v1/quote/{quoteId}/accept`, as before. Until the maker has seen the fee in a
   mined transaction it answers `402` with `code: "feeRequired"` and a `Retry-After`: wait as
   asked and post the same accept again ([below](#accepting)). Once it has seen the fee, the
   accept does what it always did: the maker opens the swap and answers with its deadlines.
6. Nothing after that changes: verify the swap on-chain, deposit, claim.

The fee is no part of the deposit: `depositZat` and `networkCost` mean what they did.

## What the wallet checks

Before anything is paid:

- **The amount.** `zat` is above zero and at most a cap the wallet sets, or its user approves. The
  reference CLI's is `--max-open-fee`, 500,000 zatoshis unless given; the reference client takes
  no fee at all until its caller names a cap.
- **The address.** A unified address of the wallet's own Zcash network, with an Orchard receiver
  and no other, and no metadata: a wallet's own choice of pool then can't send the fee in the
  clear. Refuse any other.
- **The time left.** At least 60 seconds of the quote's life (`expiresAt`), judged before the
  payment is built and again right before it is sent: building one can take a sync and a proof.
  The fee counts only once mined, and a payment sent later than that is likely lost.
- **The funds.** The wallet holds the fee, the deposit and both payments' transaction fees. A fee
  paid by a wallet that cannot then deposit buys nothing.

A quote that fails a check is dropped. Its index is spent all the same: the request for it
carried the swap's address and note.

## When the fee is lost

The fee is the maker's from the moment it is paid. Nothing is sent back, and nothing carries
over to another quote. It buys no swap when:

- it is mined after the quote expired, or the maker first sees it then;
- it is paid short of `zat`;
- the wallet never accepts, or the maker turns the accept down;
- the swap opens and the user deposits nothing, or never claims.

A maker that takes the fee and never opens the swap keeps it too: that is the trust the fee asks,
and why the wallet caps it.

## Accepting

Post the accept only once the fee is mined. Then:

- **`402`, `feeRequired`.** The maker has not seen the fee yet. Wait what `Retry-After` asks (the
  reference client waits at least a second, and five where the maker names no wait) and post the
  same accept again, until `expiresAt` by the wallet's clock. After that stop: the quote is gone,
  and the fee with it. From the second wait on, wait at least 12 seconds: a gateway lets each
  caller a few accepts a minute through, and holds one past them back half a minute, which a
  quote near its end may not have.
- **Any other `4xx`.** The accept is turned down and nothing will open this quote's swap. A fee
  paid for it is lost.
- **A `5xx`, or no answer.** The accept may have opened the swap. Post it again: an accept
  repeated with the same keys answers with the same swap. Never pay again. Wait what
  `Retry-After` asks first, which from a gateway holding a caller back is half a minute, and
  12 seconds where none is named. Once `expiresAt` has passed, read the contract instead: the
  swap's id follows from the maker's address and the wallet's own share, and the chain shows
  whether it opened.

## Persistence

Save, each before the step that follows it: the swap's index and the quote, before the fee is
built; the fee's transaction id, before it is sent; the swap, once accepted. On a restart:

- With a transaction id saved, never build another payment. If it is unmined and the quote has
  its 60 seconds left, send the saved one again; with less left, send nothing more. Either way
  wait for it until the quote expires, and accept once it is mined.
- If the quote has expired with the fee unmined, give the swap up. Should the payment be mined
  after all, the fee is lost.
- With the quote saved and no payment built, pay only if the time left still allows; else drop
  the quote, with nothing paid.

A wallet whose funds are one note spends that note on the fee, and its deposit then waits for the
change to be spendable. Paying the fee as soon as the quote is checked leaves the most time.

## Reference

`zecswap_client::User` has the flow: `quote` takes and checks a quote, `Quoted::fee_due` names
the fee to pay with the time left, and `accept` posts until the maker has seen it, failing with
`FeeNotHonoured` if the quote expires first. `open` does both at once and refuses a quote that
asks a fee. The CLI's `swap` (`crates/zecswap-cli/src/swap.rs`) shows the persistence. The
Android library has the check of amount and address as `AtomicSwap.checkOpenFee`.
