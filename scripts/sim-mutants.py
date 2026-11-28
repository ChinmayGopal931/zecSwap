#!/usr/bin/env python3
import concurrent.futures
import hashlib
import json
import os
import pathlib
import queue
import shutil
import signal
import subprocess
import sys
import threading

ROOT = pathlib.Path(__file__).resolve().parent.parent
SEEDS = sys.argv[1] if len(sys.argv) > 1 else "0..150"
ONLY = set(sys.argv[2:])
WORKERS = int(os.environ.get("ZECSWAP_SIM_WORKERS", "1"))
CORES = os.cpu_count() or 4
SHARED = {"CARGO_BUILD_JOBS": CORES, "ZECSWAP_SIM_THREADS": (CORES + 1) // 2}
BUILD = {"CARGO_INCREMENTAL": "1"}
COPIED = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo", "crates", "contracts/out")
CLONE = ["cp", "-Rpc"] if sys.platform == "darwin" else ["cp", "-Rp", "--reflink=auto"]
JUDGE = "crates/zecswap-chain/src/evm/judge.rs"
EVM = "crates/zecswap-chain/src/evm.rs"
USER = "crates/zecswap-client/src/user.rs"
REVERSE = "crates/zecswap-client/src/reverse.rs"
DEPOSITS = "crates/zecswap-client/src/deposits.rs"
CHAIN_DEPOSITS = "crates/zecswap-chain/src/evm/deposits.rs"
AUTH = "crates/zecswap-core/src/auth.rs"
WALLET = "crates/zecswap-chain/src/zcash/wallet.rs"
LIGHTWALLETD = "crates/zecswap-chain/src/zcash/lightwalletd.rs"
SWEEP = "crates/zecswap-core/src/sweep.rs"
SYNC = "crates/zecswap-chain/src/zcash/wallet/sync.rs"
ALERT = "crates/zecswap-chain/src/alert.rs"
RAILGUN = "crates/zecswap-chain/src/evm/railgun.rs"
FUNDING = "crates/zecswap-chain/src/evm/funding.rs"
CANONICAL = "crates/zecswap-chain/src/evm/canonical.rs"
CLI_SWAP = "crates/zecswap-cli/src/swap.rs"
CLI_DEPOSIT = "crates/zecswap-cli/src/deposit.rs"
BACKUP = pathlib.Path("target") / "sim-mutants"
RUNNING = set()
HALT = threading.Event()
FORWARD = ("survey", "seed ")
BACKWARD = ("reverse_survey", "reverse seed ")
DEPOSIT = ("deposit_survey", "deposit seed ")
ZCASH = ("zcash_survey", "zcash seed ")
GUARD = ("guard_survey", "guard seed ")
COMMAND = ("cli_survey", "cli seed ")
SOUND = {}

READY_SENT = """        self.relayer
            .ready_reverse(
                &Authorization {
                    swap_id: swap.swap_id,
                    terms: (&self.terms(swap)?).into(),
                    deadline,
                    signature: signature.into(),
                },
                swap.ticket.as_deref(),
            )
            .await?;
"""
READY_SAVED = """        swap.ready_signed_until = Some(swap.ready_signed_until.unwrap_or(0).max(deadline));
        save(swap)?;
"""
REFUND_SENT = """        self.relayer
            .refund_reverse(
                &Refund {
                    swap_id: swap.swap_id,
                    terms: escrow,
                    secret: keys.share.to_be_bytes().into(),
                    payout,
                },
                swap.ticket.as_deref(),
            )
            .await?;
"""
REFUND_SAVED = """        swap.refunding = true;
        save(swap)?;
"""

MUTANTS = {
    "ready-depth-0": (FORWARD, [
        (JUDGE, "pub const READY_DEPTH: u64 = 2;", "pub const READY_DEPTH: u64 = 0;"),
    ]),
    "no-claim-margin": (FORWARD, [
        (
            JUDGE,
            "    if !window.leaves(now.max(wall), margin) {\n        return Err(Refusal::TooSoon);\n    }\n",
            "",
        ),
        (
            EVM,
            "if !window.leaves(head.timestamp.max(wall_now()), *margin) {",
            "if window == Window::Shut {",
        ),
    ]),
    "no-deadline-check": (FORWARD, [
        (
            USER,
            "        check_deadlines(now, self.min_time_to_t0, chain.t0, chain.t1)?;\n",
            "        let _ = now;\n",
        ),
    ]),
    "deadline-check-panics": (FORWARD, [
        (
            USER,
            "        check_deadlines(now, self.min_time_to_t0, chain.t0, chain.t1)?;\n",
            "        check_deadlines(now, self.min_time_to_t0, chain.t0, chain.t1).unwrap();\n",
        ),
    ]),
    "any-terms": (FORWARD, [
        (EVM, "    if state.terms_hash != terms.hash() {", "    if false {"),
    ]),
    "any-open-fee": (FORWARD, [
        (USER, "        fee.zat <= max,\n", "        fee.zat <= max || true,\n"),
    ]),
    "claim-any-time": (FORWARD, [
        (
            EVM,
            "open(stage == Stage::Ready || now >= t0, Window::Until(t1)),",
            "open(true, Window::Until(t1)),",
        ),
        (
            JUDGE,
            "    Ok(if now < t0 {\n        Go::OnceReadyAt(head.number.saturating_sub(READY_DEPTH))\n    } else {\n        Go::Now\n    })",
            "    let _ = t0;\n    Ok(Go::Now)",
        ),
    ]),
    "claim-in-the-makers-hour": (FORWARD, [
        (
            EVM,
            "(Reveal::Claim, false) if now >= t1 => open(now >= late, Window::Lasting),",
            "(Reveal::Claim, false) if now >= t1 => open(true, Window::Lasting),",
        ),
        (
            JUDGE,
            "            Stage::Open | Stage::Ready if now >= t1 => {\n                Refusal::MakersUntil(t1.saturating_add(windows.late))\n            }\n",
            "",
        ),
    ]),
    "ready-after-refund": (BACKWARD, [
        (
            REVERSE,
            "        ensure!(\n            !swap.refunding,\n            \"a refund was asked for; the escrow is never readied after it\"\n        );\n",
            "",
        ),
    ]),
    "ready-after-refund-panics": (BACKWARD, [
        (
            REVERSE,
            "        ensure!(\n            !swap.refunding,\n",
            "        assert!(\n            !swap.refunding,\n",
        ),
    ]),
    "ready-short": (BACKWARD, [
        (
            REVERSE,
            "        ensure!(\n            spendable >= swap.quote.terms.deposit_zat,\n            \"ZEC deposit is not confirmed in full\"\n        );\n",
            "        let _ = spendable;\n",
        ),
    ]),
    "ready-near-t0": (BACKWARD, [
        (
            JUDGE,
            "    if head.timestamp.saturating_add(ttl) >= t0 {\n        return Err(Refusal::TooSoon);\n    }\n",
            "    let _ = (t0, head, ttl);\n",
        ),
    ]),
    "ready-saved-late": (BACKWARD, [
        (REVERSE, READY_SAVED + READY_SENT, READY_SENT + READY_SAVED),
    ]),
    "refund-while-a-ready-may-land": (BACKWARD, [
        (JUDGE, "    if signed || ready_may_land {", "    if ready_may_land || signed && false {"),
    ]),
    "refund-before-t1": (BACKWARD, [
        (
            JUDGE,
            "    if windows.window(Reveal::Refund, true, stage, (t0, t1), now) == Window::Shut {\n        return Err(Refusal::Shut);\n    }\n",
            "",
        ),
    ]),
    "refund-saved-late": (BACKWARD, [
        (REVERSE, REFUND_SAVED + REFUND_SENT, REFUND_SENT + REFUND_SAVED),
    ]),
    "quote-any-deployment": (BACKWARD, [
        (
            REVERSE,
            "            quote.terms.chain_id == self.settlement.chain_id().await?\n                && quote.terms.contract == self.settlement.contract()\n                && quote.terms.token == self.token,\n",
            "            true,\n",
        ),
    ]),
    "quote-any-amounts": (BACKWARD, [
        (
            REVERSE,
            "            !quote.terms.maker.is_zero() && quote.terms.amount > 0 && quote.terms.deposit_zat > 0,\n",
            "            true,\n",
        ),
    ]),
    "quote-any-user": (BACKWARD, [
        (
            REVERSE,
            "            quote.user == Address::from(keys.auth.address())\n                && quote.refund_note == B256::from(note.commitment()),\n",
            "            true,\n",
        ),
    ]),
    "quote-any-deadlines": (BACKWARD, [
        (
            REVERSE,
            "            quote.funding_deadline < quote.ready_deadline\n                && quote.ready_deadline < quote.refund_after\n                && quote.ready_deadline - quote.funding_deadline >= windows.min\n                && quote.refund_after - quote.ready_deadline >= windows.min\n                && quote.refund_after - quote.funding_deadline <= MAX_READY_WAIT,\n",
            "            windows.min > 0,\n",
        ),
    ]),
    "quote-any-proof": (BACKWARD, [
        (
            REVERSE,
            "        self.context(swap)\n            .verify_maker(&quote.terms.maker_share, &quote.terms.maker_proof)?;\n",
            "",
        ),
    ]),
    "funding-locked-long": (BACKWARD, [
        (
            REVERSE,
            "\n                && swap.quote.refund_after <= now.saturating_add(MAX_READY_WAIT),",
            ",",
        ),
    ]),
    "maker-ready-depth-0": (BACKWARD, [
        (JUDGE, "pub const REVERSE_READY_DEPTH: u64 = 4;", "pub const REVERSE_READY_DEPTH: u64 = 0;"),
    ]),
    "maker-claim-margin": (BACKWARD, [
        (
            JUDGE,
            "    if !window.leaves(head.timestamp.max(wall), margin) {\n        return Err(Refusal::TooSoon);\n    }\n",
            "",
        ),
        (
            EVM,
            "if !window.leaves(head.timestamp.max(wall_now()), *margin) {",
            "if window == Window::Shut {",
        ),
    ]),
    "maker-claims-after-t1": (BACKWARD, [
        (
            EVM,
            "(Reveal::Claim, true) => open(now < t1 && stage == Stage::Ready, Window::Until(t1)),",
            "(Reveal::Claim, true) => open(stage == Stage::Ready, Window::Until(t1)),",
        ),
    ]),
    "user-refunds-a-ready-escrow": (BACKWARD, [
        (
            EVM,
            "(Reveal::Refund, true) => open(stage == Stage::Open || now >= t1, Window::Lasting),",
            "(Reveal::Refund, true) => open(true, Window::Lasting),",
        ),
    ]),
    "reveals-judged-forward": (BACKWARD, [
        (
            EVM,
            "let reverse = !funding.refundNote.is_zero();",
            "let reverse = funding.refundNote.is_zero() && false;",
        ),
    ]),
    "deposit-any-chain": (DEPOSIT, [
        (
            DEPOSITS,
            "        ensure!(\n            terms.chain_id == self.deployment.chain_id,\n            \"the relayer serves another chain\"\n        );\n",
            "",
        ),
    ]),
    "deposit-chain-check-panics": (DEPOSIT, [
        (
            DEPOSITS,
            "        ensure!(\n            terms.chain_id == self.deployment.chain_id,\n",
            "        assert!(\n            terms.chain_id == self.deployment.chain_id,\n",
        ),
    ]),
    "deposit-any-deployment": (DEPOSIT, [
        (
            DEPOSITS,
            "        ensure!(\n            (deposits.contract, deposits.token)\n                == (self.deployment.contract, self.deployment.token),\n            \"the relayer takes deposits for another deployment\"\n        );\n",
            "",
        ),
    ]),
    "deposit-under-the-minimum": (DEPOSIT, [
        (
            DEPOSITS,
            "        if value < terms.min_value {\n            return Ok(None);\n        }\n",
            "",
        ),
    ]),
    "deposit-any-fee": (DEPOSIT, [
        (DEPOSITS, "            fee <= cap && fee < value,\n", "            fee < value,\n"),
    ]),
    "deposit-fee-of-everything": (DEPOSIT, [
        (DEPOSITS, "            fee <= cap && fee < value,\n", "            fee <= cap,\n"),
    ]),
    "deposit-valid-too-long": (DEPOSIT, [
        (
            DEPOSITS,
            "    VALIDITY.min(terms.max_validity.saturating_sub(CLOCK_SLACK))",
            "    VALIDITY.min(terms.max_validity.max(VALIDITY))",
        ),
    ]),
    "deposit-unsaved": (DEPOSIT, [
        (
            DEPOSITS,
            "        keep(&request)?;\n        let give_up = Instant::now() + PATIENCE;\n        let (mut wait, mut signed_again) = (BACKOFF, false);\n",
            "        let give_up = Instant::now() + PATIENCE;\n        let (mut wait, mut signed_again) = (BACKOFF, false);\n",
        ),
    ]),
    "deposit-signed-again-unsaved": (DEPOSIT, [
        (
            DEPOSITS,
            "                request = self.sign_again(index).await?;\n                keep(&request)?;\n",
            "                request = self.sign_again(index).await?;\n",
        ),
    ]),
    "deposit-refused-unsaved": (DEPOSIT, [
        (
            DEPOSITS,
            "                        .with_context(|| format!(\"the relayer refused the deposit: {reason}\"))?;\n                    keep(&request)?;\n",
            "                        .with_context(|| format!(\"the relayer refused the deposit: {reason}\"))?;\n",
        ),
    ]),
    "deposit-posted-stale": (DEPOSIT, [
        (
            DEPOSITS,
            "            if request.valid_before < now() + LANDING + CLOCK_SLACK {\n                request = self.sign_again(index).await?;\n                keep(&request)?;\n            }\n",
            "",
        ),
    ]),
    "deposit-never-posted-again": (DEPOSIT, [
        (
            DEPOSITS,
            "            if Instant::now() >= deadline {\n                return Ok(None);\n            }\n",
            "",
        ),
    ]),
    "deposit-believed-of-another-address": (DEPOSIT, [
        (
            DEPOSITS,
            "                .any(|landed| landed.from == from && ours(landed.note)),",
            "                .any(|landed| ours(landed.note)),",
        ),
    ]),
    "deposit-believed-to-another-note": (DEPOSIT, [
        (
            DEPOSITS,
            "        let ours = |commitment: B256| expected.is_none_or(|expected| commitment == expected);",
            "        let ours = |commitment: B256| expected.is_none_or(|expected| commitment == expected || true);",
        ),
    ]),
    "rescue-any-fee": (DEPOSIT, [
        (
            DEPOSITS,
            "        ensure!(\n            fee <= self.cap.fee,\n            \"the relayer asks a fee of {fee}, over the {} this wallet pays\",\n            self.cap.fee\n        );\n",
            "",
        ),
    ]),
    "rescue-of-nothing": (DEPOSIT, [
        (
            DEPOSITS,
            "        if held <= fee {\n            return Ok(None);\n        }\n",
            "        let _ = held;\n",
        ),
    ]),
    "rescue-unsaved": (DEPOSIT, [
        (
            DEPOSITS,
            "        keep(&request)?;\n        let give_up = Instant::now() + PATIENCE;\n        let mut wait = BACKOFF;\n",
            "        let give_up = Instant::now() + PATIENCE;\n        let mut wait = BACKOFF;\n",
        ),
    ]),
    "recover-one-window": (DEPOSIT, [
        (
            DEPOSITS,
            "            if !found {\n                return Ok(used);\n            }\n",
            "            if !found || first == 0 {\n                return Ok(used);\n            }\n",
        ),
    ]),
    "shows-an-address-deposited-from": (DEPOSIT, [
        (
            DEPOSITS,
            "                .any(|(used, state)| *used == index && state.deposited)",
            "                .any(|(used, state)| *used == index && state.deposited && index > 0)",
        ),
    ]),
    "payout-rescue-signed-otherwise": (DEPOSIT, [
        (
            AUTH,
            "            &keccak(&[RESCUE_TYPE]),\n            id,\n            note,\n            &address(relayer),\n            &uint(fee),\n            &uint(authorization.nonce.into()),\n            &uint(authorization.deadline.into()),\n",
            "            &keccak(&[RESCUE_TYPE]),\n            id,\n            note,\n            &address(relayer),\n            &uint(fee),\n            &uint(authorization.deadline.into()),\n            &uint(authorization.nonce.into()),\n",
        ),
    ]),
    "deposited-address-unused": (DEPOSIT, [
        (
            CHAIN_DEPOSITS,
            "        self.balance > 0 || self.deposited",
            "        self.balance > 0",
        ),
    ]),
    "wallet-synced-with-no-account": (ZCASH, [
        (
            WALLET,
            "        if self.db.get_account_ids()?.is_empty() {\n            return Ok(());\n        }\n        self.check_server(client).await?;\n        let tip = lightwalletd::chain_tip(client).await?;",
            "        self.check_server(client).await?;\n        let tip = lightwalletd::chain_tip(client).await?;",
        ),
    ]),
    "wallet-synced-born-above-the-tip": (ZCASH, [
        (
            WALLET,
            "        if sync::born_above(&self.db, tip)? {\n            return Ok(());\n        }\n",
            "        let _ = tip;\n",
        ),
        (
            SYNC,
            "    if born_above(db, tip)? {\n        return Ok(false);\n    }\n",
            "    let _ = born_above;\n",
        ),
    ]),
    "second-tip-under-the-birthday": (ZCASH, [
        (
            SYNC,
            "    if born_above(db, tip)? {\n        return Ok(false);\n    }\n",
            "",
        ),
    ]),
    "stepped-back-to-any-height": (ZCASH, [
        (
            SYNC,
            "                    if state.block_height() != safe {\n",
            "                    if state.block_height() != safe && false {\n",
        ),
    ]),
    "joint-imported-twice": (ZCASH, [
        (
            WALLET,
            "        if let Some(account) = self.find_joint(joint, &ufvk)? {\n            return Ok(account);\n        }\n        self.check_server(client).await?;\n        let birthday = Self::birthday_at_tip(client).await?;\n        let purpose",
            "        self.check_server(client).await?;\n        let birthday = Self::birthday_at_tip(client).await?;\n        let purpose",
        ),
    ]),
    "any-joint-tracked": (ZCASH, [
        (
            WALLET,
            "        .map(|fvk| fvk.to_bytes())\n        == Some(joint.fvk().to_bytes())",
            "        .map(|fvk| fvk.to_bytes() == joint.fvk().to_bytes())\n        .is_some()",
        ),
    ]),
    "funds-of-one-pool": (ZCASH, [
        (
            WALLET,
            "let pools = [balance.orchard_balance(), balance.ironwood_balance()];",
            "let pools = [balance.orchard_balance(), balance.orchard_balance()];",
        ),
    ]),
    "sweep-over-the-fee-ceiling": (ZCASH, [
        (
            WALLET,
            "        if fee > SWEEP_FEE_CEILING {\n            return Err(Error::SweepFee {\n                fee,\n                ceiling: SWEEP_FEE_CEILING,\n            });\n        }\n",
            "",
        ),
    ]),
    "sweep-told-to-the-joint-key": (ZCASH, [
        (
            WALLET,
            "self.store_proposal(prover, account, &proposal, OvkPolicy::Discard, sign)",
            "self.store_proposal(prover, account, &proposal, OvkPolicy::Sender, sign)",
        ),
    ]),
    "sweep-to-a-used-address": (ZCASH, [
        (
            WALLET,
            "        let (address, _) = self\n            .db\n            .get_next_available_address(account, UnifiedAddressRequest::ORCHARD)?\n            .ok_or_else(|| Error::Wallet(\"account cannot generate an Orchard address\".into()))?;\n        Ok(address.encode(&self.network))",
            "        self.address(account)",
        ),
    ]),
    "payment-proved-unheld": (ZCASH, [
        (
            WALLET,
            "        let proposal = self.propose_payment(account, payments, &policy, Some(hold))?;",
            "        let proposal = self.propose_payment(account, payments, &policy, None)?;\n        let _ = hold;",
        ),
    ]),
    "payment-not-proved-again-from-its-notes": (ZCASH, [
        (
            WALLET,
            "        let policy =\n            SpendPolicy::default().with_locked_input_policy(LockedInputPolicy::PreferLocked(own));",
            "        let policy = SpendPolicy::default();\n        let _ = own;",
        ),
    ]),
    "payment-held-for-good": (ZCASH, [
        (
            WALLET,
            "        Ok(payment.held_until >= next && (expiry == 0 || BlockHeight::from(expiry) >= next))",
            "        Ok(payment.held_until >= payment.held_until && (expiry == 0 || BlockHeight::from(expiry) >= next))",
        ),
    ]),
    "failed-payment-keeps-its-notes": (ZCASH, [
        (
            WALLET,
            "        if stored.is_err() {\n            self.give_back(&proposal, holder);\n        }\n        stored",
            "        let _ = (&proposal, holder);\n        stored",
        ),
    ]),
    "released-payment-keeps-its-notes": (ZCASH, [
        (
            WALLET,
            "        Ok(unlock_proposal_inputs(\n            &mut self.db,\n            &payment.proposal,\n            payment.holder,\n        )?)\n    }\n\n    fn give_back(",
            "        let _ = payment;\n        Ok(())\n    }\n\n    fn give_back(",
        ),
    ]),
    "confirmed-a-block-early": (ZCASH, [
        (
            WALLET,
            "                .is_some_and(|depth| depth >= self.confirmations.untrusted().get() - 1)",
            "                .is_some_and(|depth| depth + 2 >= self.confirmations.untrusted().get())",
        ),
    ]),
    "expired-a-block-early": (ZCASH, [
        (
            WALLET,
            "                .is_some_and(|block| block.block_height() > tx.expiry_height()))",
            "                .is_some_and(|block| block.block_height() >= tx.expiry_height()))",
        ),
    ]),
    "expiry-past-the-upgrade": (ZCASH, [
        (
            WALLET,
            "    Some(default.min(upgrade - 1))\n}",
            "    let _ = upgrade;\n    Some(default)\n}",
        ),
    ]),
    "received-while-unmined": (ZCASH, [
        (
            WALLET,
            "                 WHERE to_account_uuid = ?1 AND tx_mined_height IS NOT NULL\n",
            "                 WHERE to_account_uuid = ?1\n",
        ),
    ]),
    "inventory-of-another-seed": (ZCASH, [
        (
            WALLET,
            "            None if self.has_account_named(INVENTORY)? => Err(Error::Wallet(\n                \"inventory seed does not match the wallet\".into(),\n            )),\n",
            "",
        ),
    ]),
    "own-account-made-twice": (ZCASH, [
        (
            WALLET,
            "        match self.account_of(&key)? {\n            Some(account) => Ok((account, key)),\n            None => self.create_account(client, seed, name).await,\n        }\n    }\n\n    pub async fn inventory_account(",
            "        let _ = (key, Self::account_of);\n        self.create_account(client, seed, name).await\n    }\n\n    pub async fn inventory_account(",
        ),
    ]),
    "no-deadline-on-a-call": (ZCASH, [
        (
            LIGHTWALLETD,
            "        if self.total.as_mut().poll(cx).is_ready() || self.idle.as_mut().poll(cx).is_ready() {",
            "        if self.idle.as_mut().poll(cx).is_ready() {",
        ),
    ]),
    "no-deadline-on-silence": (ZCASH, [
        (
            LIGHTWALLETD,
            "        if self.total.as_mut().poll(cx).is_ready() || self.idle.as_mut().poll(cx).is_ready() {",
            "        if self.total.as_mut().poll(cx).is_ready() {",
        ),
    ]),
    "any-network-served": (ZCASH, [
        (
            LIGHTWALLETD,
            "    if served != expected {\n        return Err(Error::Config(format!(\n            \"lightwalletd serves Zcash's {served} network, not {expected}\"\n        )));\n    }\n    Ok(())",
            "    let _ = (served, expected);\n    Ok(())",
        ),
    ]),
    "refusal-not-looked-up": (ZCASH, [
        (
            LIGHTWALLETD,
            "    if let Ok(Ok(known)) = tokio::time::timeout(Duration::from_secs(5), lookup).await\n        && !data.is_empty()\n        && known.data == data\n        && known.height != u64::MAX\n    {\n        return Ok(());\n    }\n",
            "    drop((lookup, data));\n",
        ),
    ]),
    "orphan-taken-for-held": (ZCASH, [
        (
            LIGHTWALLETD,
            "        && known.height != u64::MAX\n",
            "",
        ),
    ]),
    "any-answer-taken-for-accepted": (ZCASH, [
        (
            LIGHTWALLETD,
            "    if response.error_code == 0 {\n        return Ok(());\n    }\n    if let Ok(Ok(known))",
            "    if response.error_code == 0 || !response.error_message.is_empty() {\n        return Ok(());\n    }\n    if let Ok(Ok(known))",
        ),
    ]),
    "sweep-signed-to-any-receiver": (ZCASH, [
        (
            SWEEP,
            "                if output.recipient().as_ref() != Some(&self.recipient) {\n                    return Err(OrchardError::Custom(policy(\n                        \"sweep output or change goes to another receiver\",\n                    )));\n                }\n",
            "",
        ),
    ]),
    "sweep-signed-at-any-fee": (ZCASH, [
        (
            SWEEP,
            "            || totals.fee < 0\n            || totals.fee > i128::from(self.maximum_fee)\n",
            "            || totals.fee < 0\n",
        ),
    ]),
    "sweep-signed-for-any-amount": (ZCASH, [
        (
            SWEEP,
            "        if totals.received < u128::from(self.minimum_received)\n            || totals.fee < 0",
            "        if totals.fee < 0",
        ),
    ]),
    "sweep-signed-unreadable-by-its-receiver": (ZCASH, [
        (
            SWEEP,
            "                verify_ciphertext(action, pool).map_err(OrchardError::Custom)?;\n",
            "",
        ),
    ]),
    "sweep-signed-to-a-forged-receiver": (ZCASH, [
        (
            SWEEP,
            "            output.verify_note_commitment(spend)?;\n",
            "",
        ),
        (
            SWEEP,
            "                verify_ciphertext(action, pool).map_err(OrchardError::Custom)?;\n",
            "",
        ),
    ]),
    "stuck-private-sender-unalerted": (BACKWARD, [
        (EVM, "            watch.raised.zip(dropped)\n", "            watch.raised.zip(dropped).filter(|_| false)\n"),
    ]),
    "private-alert-with-no-send-dropped": (BACKWARD, [
        (
            EVM,
            "            watch.raised.zip(dropped)\n",
            """            watch
                .raised
                .filter(|_| chain < signing)
                .map(|raised| (raised, dropped.unwrap_or(raised.0)))
""",
        ),
    ]),
    "private-alert-at-the-next-send": (BACKWARD, [
        (
            EVM,
            "                    if let Ok(counted) = self.pending_nonce().await {\n",
            "                    if let Ok(counted) = self.pending_nonce().await && false {\n",
        ),
    ]),
    "failing-sink-let-through": (DEPOSIT, [
        (
            ALERT,
            "    if catch_unwind(AssertUnwindSafe(|| alerts(&alert))).is_err() {\n",
            "    alerts(&alert);\n    if catch_unwind(AssertUnwindSafe(|| ())).is_err() {\n",
        ),
    ]),
    "queued-sender-unalerted": (DEPOSIT, [
        (
            EVM,
            "                    if told != Some(behind) && head >= since + QUEUED_BLOCKS {\n",
            "                    if told != Some(behind) && head >= since + QUEUED_BLOCKS && false {\n",
        ),
    ]),
    "queue-alert-at-once": (DEPOSIT, [
        (
            EVM,
            "                    if told != Some(behind) && head >= since + QUEUED_BLOCKS {\n",
            "                    if told != Some(behind) && head >= since {\n",
        ),
    ]),
    "queue-alert-while-draining": (DEPOSIT, [
        (
            EVM,
            "                    let held = queued.filter(|(most, _)| behind <= *most);\n",
            "                    let held = queued.filter(|(most, _)| behind <= *most || true);\n",
        ),
    ]),
    "refused-rewind-kept-refused": (ZCASH, [
        (SYNC, "                }) if safe + 1 < at => {\n", "                }) if safe + 1 < at && false => {\n"),
    ]),
    "wallet-alert-on-any-reorg": (ZCASH, [
        (
            SYNC,
            "            let kept = kept.inspect_err(|refusal| {\n",
            """            alert::raise(
                watch.alerts,
                Alert::WalletRewindRefused {
                    wallet: watch.wallet.to_owned(),
                    reorg_at: at.into(),
                    rewind: rewind.into(),
                    refusal: String::new(),
                },
            );
            let kept = kept.inspect_err(|refusal| {
""",
        ),
    ]),
    "send-to-any-destination": (GUARD, [
        (RAILGUN, 'to == self.railgun && value == 0,', 'true,'),
    ]),
    "send-of-any-size": (GUARD, [
        (RAILGUN, '            data.len() <= MAX_CALLDATA_BYTES,\n            "the calldata is too large",', '            true,\n            "the calldata is too large",'),
    ]),
    "send-spending-nothing": (GUARD, [
        (RAILGUN, '                !tx.nullifiers.is_empty() && !tx.commitments.is_empty(),\n                "each Railgun transaction must spend notes",', '                true,\n                "each Railgun transaction must spend notes",'),
    ]),
    "send-for-any-chain": (GUARD, [
        (RAILGUN, 'require(bound.chainID == chain_id, "a proof is for another chain")?;', 'require(true, "a proof is for another chain")?;'),
    ]),
    "send-bound-to-an-adapter": (GUARD, [
        (RAILGUN, 'bound.adaptContract.is_zero() && bound.adaptParams.is_zero(),', 'true,'),
    ]),
    "send-redirected": (GUARD, [
        (RAILGUN, '                bound.unshield <= 1,\n', '                true,\n'),
    ]),
    "send-unshielding-any-token": (GUARD, [
        (RAILGUN, 'bound.unshield == 0 || erc20(&tx.unshieldPreimage.token, self.token),', 'true,'),
    ]),
    "send-for-any-fee": (GUARD, [
        (RAILGUN, '            paid >= self.fee,\n', '            true,\n'),
    ]),
    "fee-in-any-token": (GUARD, [
        (RAILGUN, '(received.token == token.into_word().0).then_some(received.value)', 'Some(received.value)'),
    ]),
    "sent-over-the-gas-cap": (GUARD, [
        (RAILGUN, '            gas_price <= policy.max_gas_price_wei,\n', '            true,\n'),
    ]),
    "sent-uncovered": (GUARD, [
        (RAILGUN, '            covers(estimate, gas_price),\n', '            true,\n'),
    ]),
    "spent-unseen": (GUARD, [
        (RAILGUN, '                if self.spends_spent_notes(transact).await? {', '                if self.spends_spent_notes(transact).await? && false {'),
    ]),
    "fourteen-nullifiers": (GUARD, [
        (RAILGUN, 'const MAX_NULLIFIERS: usize = 13;', 'const MAX_NULLIFIERS: usize = 14;'),
    ]),
    "fourteen-commitments": (GUARD, [
        (RAILGUN, 'const MAX_COMMITMENTS: usize = 13;', 'const MAX_COMMITMENTS: usize = 14;'),
    ]),
    "seventeen-transactions": (GUARD, [
        (RAILGUN, 'pub const MAX_TRANSACTIONS: usize = 16;', 'pub const MAX_TRANSACTIONS: usize = 17;'),
    ]),
    "longer-annotations": (GUARD, [
        (RAILGUN, 'const MAX_ANNOTATION_BYTES: usize = 62;', 'const MAX_ANNOTATION_BYTES: usize = 63;'),
    ]),
    "longer-memos": (GUARD, [
        (RAILGUN, 'const MAX_MEMO_BYTES: usize = 1024;', 'const MAX_MEMO_BYTES: usize = 1025;'),
    ]),
    "larger-calldata": (GUARD, [
        (RAILGUN, 'pub const MAX_CALLDATA_BYTES: usize = 64 * 1024;', 'pub const MAX_CALLDATA_BYTES: usize = 128 * 1024;'),
    ]),
    "unknown-read-as-pending": (GUARD, [
        (RAILGUN, '                None => Status::Unknown,', '                None => Status::Pending,'),
    ]),
    "nonce-taken-at-the-tip": (GUARD, [
        (RAILGUN, 'let Some(block) = latest.checked_sub(depth) else {', 'let Some(block) = latest.checked_sub(depth.min(0)) else {'),
    ]),
    "any-offset": (GUARD, [
        (CANONICAL, '                if self.word(head)? != U256::from(end - start) {', '                if self.word(head)? != U256::from(end - start) && false {'),
    ]),
    "trailing-bytes": (GUARD, [
        (CANONICAL, '    if end != data.len() {', '    if end > data.len() {'),
    ]),
    "dirty-padding": (GUARD, [
        (CANONICAL, '                if padded[len..].iter().any(|&byte| byte != 0) {', '                if padded[len..].iter().any(|&byte| byte != 0) && false {'),
    ]),
    "uncapped-lengths": (GUARD, [
        (CANONICAL, '        if len > U256::from(max) {', '        if len > U256::from(max.max(4096)) {'),
    ]),
    "uncapped-ciphertexts": (GUARD, [
        (CANONICAL, '                let len = self.len(at, self.cap)?;', '                let len = self.len(at, self.cap.max(64))?;'),
    ]),
    "funding-to-any-destination": (GUARD, [
        (FUNDING, 'to == self.relay_adapt && value == 0,', 'true,'),
    ]),
    "funding-without-success": (GUARD, [
        (FUNDING, '            action.requireSuccess,\n', '            true,\n'),
    ]),
    "funding-any-minimum-gas": (GUARD, [
        (FUNDING, 'action.minGasLimit <= U256::from(self.max_gas_limit),', 'true,'),
    ]),
    "funding-any-call-count": (GUARD, [
        (FUNDING, 'action.calls.len() == 4,', 'true,'),
    ]),
    "funding-calls-with-eth": (GUARD, [
        (FUNDING, 'action.calls.iter().all(|c| c.value.is_zero()),', 'true,'),
    ]),
    "funding-opens-any-contract": (GUARD, [
        (FUNDING, '                && open_call.to == Address::from(domain.contract)\n', ''),
    ]),
    "funding-calls-any-target": (GUARD, [
        (FUNDING, '            approve_call.to == self.token\n                && open_call.to == Address::from(domain.contract)\n                && fee_call.to == self.token\n                && shield_call.to == self.relay_adapt,', '            true,'),
    ]),
    "funding-fee-to-anyone": (GUARD, [
        (FUNDING, 'payment.to == self.fee_recipient && payment.amount', 'payment.amount'),
    ]),
    "funding-any-fee": (GUARD, [
        (FUNDING, 'payment.to == self.fee_recipient && payment.amount >= U256::from(self.fee),', 'payment.to == self.fee_recipient,'),
    ]),
    "funding-any-maker-token-user": (GUARD, [
        (FUNDING, 'words.maker == self.maker && words.token == self.token && !words.user.is_zero(),', 'true,'),
    ]),
    "funding-any-amount-or-note": (GUARD, [
        (FUNDING, 'words.amount > 0 && words.amount < (1u128 << 120) && !words.refundNote.is_zero(),', 'true,'),
    ]),
    "funding-any-deadlines": (GUARD, [
        (FUNDING, 'words.deadline < words.t0 && words.t0 < words.t1,', 'true,'),
    ]),
    "funding-approves-anyone": (GUARD, [
        (FUNDING, 'approve.spender == open_call.to && approve.amount', 'approve.amount'),
    ]),
    "funding-approves-any-amount": (GUARD, [
        (FUNDING, 'approve.spender == open_call.to && approve.amount == U256::from(words.amount),', 'approve.spender == open_call.to,'),
    ]),
    "funding-any-shield-count": (GUARD, [
        (FUNDING, '            shield._shieldRequests.len() == 1,\n', '            true,\n'),
    ]),
    "funding-any-dust-shield": (GUARD, [
        (FUNDING, 'erc20(&dust.token, self.token) && dust.value.is_zero() && !dust.npk.is_zero(),', 'true,'),
    ]),
    "funding-any-swap-id": (GUARD, [
        (FUNDING, 'id == reverse_swap_id(words.user, &terms.maker_share),', 'true,'),
    ]),
    "funding-any-signer": (GUARD, [
        (FUNDING, 'signer(&domain.open_reverse(&terms), signature) == Some(terms.user),', 'signer(&domain.open_reverse(&terms), signature) == Some(terms.user) || true,'),
    ]),
    "funding-spending-nothing": (GUARD, [
        (FUNDING, '                !tx.nullifiers.is_empty() && !tx.commitments.is_empty(),\n', '                true,\n'),
    ]),
    "funding-unbound": (GUARD, [
        (FUNDING, '                bound.chainID == domain.chain_id\n                    && bound.adaptContract == self.relay_adapt\n                    && bound.adaptParams == adapt_params,', '                true,'),
    ]),
    "funding-redirected": (GUARD, [
        (FUNDING, '                bound.unshield <= 1,\n', '                true,\n'),
    ]),
    "funding-unshield-anywhere": (GUARD, [
        (FUNDING, '                    unshield.npk == self.relay_adapt.into_word()\n                        && erc20(&unshield.token, self.token)\n                        && !unshield.value.is_zero(),', '                    true,'),
    ]),
    "funding-without-unshield": (GUARD, [
        (FUNDING, '        require(unshields > 0, "funding must unshield from Railgun")?;\n', ''),
    ]),
    "config-any-addresses": (GUARD, [
        (FUNDING, '!self.relay_adapt.is_zero() && !self.token.is_zero() && !self.maker.is_zero(),', 'true,'),
    ]),
    "config-relayer-as-maker": (GUARD, [
        (FUNDING, '            self.maker != account,\n', '            true,\n'),
    ]),
    "config-fee-elsewhere": (GUARD, [
        (FUNDING, '            self.fee_recipient == account,\n', '            true,\n'),
    ]),
    "config-no-gas": (GUARD, [
        (FUNDING, 'self.max_gas_limit > 0 && self.max_gas_price_wei > 0,', 'true,'),
    ]),
    "adapter-of-any-railgun": (GUARD, [
        (FUNDING, '!railgun.is_zero() && actual == railgun,', 'true,'),
    ]),
    "funding-past-its-deadline": (GUARD, [
        (FUNDING, 'self.now().await? < terms.deadline,', 'true,'),
    ]),
    "funding-over-the-gas-cap": (GUARD, [
        (FUNDING, 'fees.max_fee_per_gas <= policy.max_gas_price_wei,', 'true,'),
    ]),
    "funding-over-another-note": (GUARD, [
        (FUNDING, 'funding.is_some_and(|f| f.refund_note.0 == terms.refund_note),', 'funding.is_some(),'),
    ]),
    "funding-over-a-rival-escrow": (GUARD, [
        (FUNDING, '            Err(Error::WrongTerms(_)) => return Err(FundingError::Rejected(mismatch)),', '            Err(Error::WrongTerms(_)) => {}'),
    ]),
    "cli-index-unsaved": (COMMAND, [
        (CLI_SWAP, "    store.save(NEXT_INDEX, &(index + 1))?;\n", ""),
    ]),
    "cli-fee-unsaved": (COMMAND, [
        (
            CLI_SWAP,
            "                    fee: Some(*txid.as_ref()),\n                };\n                wallet.store().save(STATE, run)?;\n",
            "                    fee: Some(*txid.as_ref()),\n                };\n",
        ),
    ]),
    "cli-fee-never-sent-again": (COMMAND, [
        (CLI_SWAP, "if waited.is_none_or(|polls| polls >= FEE_RESEND) {", "if waited.is_none() {"),
    ]),
    "cli-no-funds-check": (COMMAND, [
        (CLI_SWAP, "                if held < needed {", "                if held < needed && false {"),
    ]),
    "cli-no-cancelled-shortcut": (COMMAND, [
        (
            CLI_SWAP,
            "if resumed && run.deposit.is_none() && user.state(&swap).await?.stage == Stage::Refunded {",
            "if false {",
        ),
    ]),
    "cli-joint-unsaved": (COMMAND, [
        (
            CLI_SWAP,
            "        run.joint_account = Some(account);\n        ctx.store.save(STATE, run)?;\n",
            "        run.joint_account = Some(account);\n",
        ),
    ]),
    "cli-deposit-unsaved": (COMMAND, [
        (
            CLI_SWAP,
            "    run.deposit = Some(*txid.as_ref());\n    ctx.store.save(STATE, run)?;\n    ctx.broadcast(txid).await?;\n",
            "    run.deposit = Some(*txid.as_ref());\n    ctx.broadcast(txid).await?;\n",
        ),
    ]),
    "cli-deposit-sent-first": (COMMAND, [
        (
            CLI_SWAP,
            "    run.deposit = Some(*txid.as_ref());\n    ctx.store.save(STATE, run)?;\n    ctx.broadcast(txid).await?;\n",
            "    run.deposit = Some(*txid.as_ref());\n    ctx.broadcast(txid).await?;\n    ctx.store.save(STATE, run)?;\n",
        ),
    ]),
    "cli-deposit-not-sent-again": (COMMAND, [
        (CLI_SWAP, "            if let Err(e) = ctx.broadcast(txid).await {", "            if let Err(e) = Ok::<(), anyhow::Error>(()) {"),
    ]),
    "cli-nothing-forgotten": (COMMAND, [
        (CLI_SWAP, "        ctx.wallet.forget(account)?;\n", "        let _ = account;\n"),
    ]),
    "cli-unfinished-runs-replaced": (COMMAND, [
        (CLI_SWAP, "        && !run.finished\n", "        && run.finished\n"),
    ]),
    "cli-other-versions-overwritten": (COMMAND, [
        (CLI_SWAP, "    let saved = store.load::<Run>(STATE).with_context(|| {", "    let saved = store.load::<Run>(STATE).or_else(|_| Ok::<_, anyhow::Error>(None)).with_context(|| {"),
    ]),
    "cli-request-unsaved": (COMMAND, [
        (CLI_DEPOSIT, "            store.save(PENDING, &pending)\n", "            let _ = pending;\n            Ok(())\n"),
    ]),
    "cli-landed-request-kept": (COMMAND, [
        (CLI_DEPOSIT, "                report(index, &deposit);\n                store.remove(PENDING)?;\n", "                report(index, &deposit);\n"),
    ]),
}


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


class Checkout:
    def __init__(self, root, env, apart):
        self.root = root
        self.env = env
        self.apart = apart
        self.backup = root / BACKUP
        self.undo = self.backup / "undo.json"

    def survey(self, test, prefix):
        if HALT.is_set():
            sys.exit("stopped")
        run = subprocess.Popen(
            ["cargo", "test", "-p", "zecswap-sim", "--test", "swaps", test, "--", "--exact", "--ignored", "--nocapture"],
            cwd=self.root,
            env={**BUILD, **self.env, "ZECSWAP_SIM_SEEDS": SEEDS},
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=self.apart,
        )
        RUNNING.add(run)
        try:
            out, err = run.communicate()
        except BaseException:
            run.kill()
            run.wait()
            raise
        finally:
            RUNNING.discard(run)
        lines = [line for line in out.splitlines() if line.startswith(prefix)]
        if not lines:
            sys.exit(err[-3000:])
        failing = [line for line in lines if not line.rstrip().endswith("|")]
        return len(lines), {line.split(":")[0].removeprefix(prefix): line.rsplit(" | ", 1)[-1] for line in failing}

    def put(self, path, text):
        new = self.backup / "new"
        new.write_text(text)
        if path.exists():
            shutil.copymode(path, new)
        os.replace(new, path)

    def restore(self):
        if not self.undo.exists():
            return False
        theirs = []
        for path, (original, mutated) in json.loads(self.undo.read_text()).items():
            source = self.root / path
            now = source.read_text() if source.exists() else None
            if now is not None and digest(now) == mutated:
                self.put(source, original)
            elif now == original:
                os.utime(source)
            else:
                theirs.append(path)
        if theirs:
            sys.exit(
                f"not undone, as edited since the mutant went in: {' '.join(theirs)}. "
                f"Take the mutant out by hand ({self.undo} holds each file as it was), then delete {self.undo}"
            )
        self.undo.unlink()
        return True

    def edited(self, name, edits):
        files = {}
        for path, old, new in edits:
            text = files[path] if path in files else (self.root / path).read_text()
            if text.count(old) != 1:
                sys.exit(f"{name}: {path} no longer has the line this mutant changes")
            files[path] = text.replace(old, new)
        return files

    def mutant(self, name):
        (test, prefix), edits = MUTANTS[name]
        files = self.edited(name, edits)
        held = {path: ((self.root / path).read_text(), digest(text)) for path, text in files.items()}
        try:
            self.put(self.undo, json.dumps(held))
            for path, text in files.items():
                self.put(self.root / path, text)
            return self.survey(test, prefix)
        finally:
            self.restore()

    def copy(self, number, of):
        root = self.backup / f"worker-{number}"
        built = [f"target/{entry.name}" for entry in (self.root / "target").iterdir() if entry.name != BACKUP.name]
        for part in (*COPIED, *built):
            if (self.root / part).exists():
                (root / part).parent.mkdir(parents=True, exist_ok=True)
                subprocess.run([*CLONE, self.root / part, root / part], check=True)
        (root / BACKUP).mkdir(parents=True)
        share = {name: str(max(1, int(self.env.get(name, whole)) // of)) for name, whole in SHARED.items()}
        return Checkout(root, {**self.env, **share, "CARGO_TARGET_DIR": str(root / "target")}, True)


def stopped(number, _):
    sys.exit(128 + number)


def told(name, ran, seeds, unchanged, escaped):
    test = MUTANTS[name][0][0]
    if test not in SOUND:
        SOUND[test] = unchanged(MUTANTS[name][0])
        if SOUND[test]:
            print(f"{test}: seeds failing unchanged, counted only where they fail otherwise: {' '.join(sorted(SOUND[test], key=int))}", flush=True)
    seeds = [seed for seed, how in seeds.items() if SOUND[test].get(seed) != how]
    print(f"{name}: caught by {len(seeds)} of {ran} seeds ({test}): {' '.join(seeds[:12])}", flush=True)
    if not seeds:
        escaped.append(name)


def in_place(home, chosen, escaped):
    unchanged = {survey: home.survey(*survey)[1] for survey in dict.fromkeys(MUTANTS[name][0] for name in chosen)}
    for name in chosen:
        ran, seeds = home.mutant(name)
        told(name, ran, seeds, unchanged.get, escaped)


def in_copies(home, chosen, escaped):
    copies = queue.Queue()

    def on_a_copy(work):
        def run():
            copy = copies.get()
            try:
                return work(copy)
            finally:
                copies.put(copy)

        return run

    workers = max(1, min(WORKERS, len(chosen)))
    pool = concurrent.futures.ThreadPoolExecutor(workers)
    try:
        for number in range(workers):
            copies.put(home.copy(number, workers))
        unchanged = {
            survey: pool.submit(on_a_copy(lambda copy, survey=survey: copy.survey(*survey)[1]))
            for survey in dict.fromkeys(MUTANTS[name][0] for name in chosen)
        }
        mutants = [pool.submit(on_a_copy(lambda copy, name=name: copy.mutant(name))) for name in chosen]
        for name, mutant in zip(chosen, mutants):
            ran, seeds = mutant.result()
            told(name, ran, seeds, lambda survey: unchanged[survey].result(), escaped)
    finally:
        HALT.set()
        pool.shutdown(wait=False, cancel_futures=True)
        for run in list(RUNNING):
            try:
                os.killpg(run.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        pool.shutdown()
        left(home)


def left(home):
    for copy in home.backup.glob("worker-*"):
        shutil.rmtree(copy, ignore_errors=True)


def main():
    for number in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, stopped)
    home = Checkout(ROOT, os.environ, False)
    home.backup.mkdir(parents=True, exist_ok=True)
    if home.restore():
        print("undid the mutant an earlier run left in place", flush=True)
    (home.backup / "new").unlink(missing_ok=True)
    left(home)
    chosen = [name for name in MUTANTS if not ONLY or name in ONLY]
    escaped = []
    if WORKERS > 1:
        in_copies(home, chosen, escaped)
    else:
        in_place(home, chosen, escaped)
    if escaped:
        sys.exit(f"escaped the simulation: {' '.join(escaped)}")


main()
