#!/usr/bin/env python3
import hashlib
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SEEDS = sys.argv[1] if len(sys.argv) > 1 else "0..150"
ONLY = set(sys.argv[2:])
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
BACKUP = ROOT / "target" / "sim-mutants"
UNDO = BACKUP / "undo.json"
FORWARD = ("survey", "seed ")
BACKWARD = ("reverse_survey", "reverse seed ")
DEPOSIT = ("deposit_survey", "deposit seed ")
ZCASH = ("zcash_survey", "zcash seed ")
OPEN = {"zcash_survey"}
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
}


def survey(test, prefix):
    run = subprocess.run(
        ["cargo", "test", "-p", "zecswap-sim", "--test", "swaps", test, "--", "--exact", "--ignored", "--nocapture"],
        cwd=ROOT,
        env={**os.environ, "ZECSWAP_SIM_SEEDS": SEEDS},
        capture_output=True,
        text=True,
    )
    lines = [line for line in run.stdout.splitlines() if line.startswith(prefix)]
    if not lines:
        sys.exit(run.stderr[-3000:])
    failing = [line for line in lines if not line.rstrip().endswith("|")]
    return len(lines), {line.split(":")[0].removeprefix(prefix): line.rsplit(" | ", 1)[-1] for line in failing}


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def put(path, text):
    new = BACKUP / "new"
    new.write_text(text)
    if path.exists():
        shutil.copymode(path, new)
    os.replace(new, path)


def restore():
    if not UNDO.exists():
        return False
    theirs = []
    for path, (original, mutated) in json.loads(UNDO.read_text()).items():
        source = ROOT / path
        now = source.read_text() if source.exists() else None
        if now is not None and digest(now) == mutated:
            put(source, original)
        elif now == original:
            os.utime(source)
        else:
            theirs.append(path)
    if theirs:
        sys.exit(
            f"not undone, as edited since the mutant went in: {' '.join(theirs)}. "
            f"Take the mutant out by hand ({UNDO} holds each file as it was), then delete {UNDO}"
        )
    UNDO.unlink()
    return True


def edited(name, edits):
    files = {}
    for path, old, new in edits:
        text = files[path] if path in files else (ROOT / path).read_text()
        if text.count(old) != 1:
            sys.exit(f"{name}: {path} no longer has the line this mutant changes")
        files[path] = text.replace(old, new)
    return files


def stopped(number, _):
    sys.exit(128 + number)


def main():
    for number in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, stopped)
    BACKUP.mkdir(parents=True, exist_ok=True)
    if restore():
        print("undid the mutant an earlier run left in place", flush=True)
    (BACKUP / "new").unlink(missing_ok=True)
    escaped = []
    for name, ((test, prefix), edits) in MUTANTS.items():
        if ONLY and name not in ONLY:
            continue
        files = edited(name, edits)
        held = {path: ((ROOT / path).read_text(), digest(text)) for path, text in files.items()}
        try:
            put(UNDO, json.dumps(held))
            for path, text in files.items():
                put(ROOT / path, text)
            ran, seeds = survey(test, prefix)
        finally:
            restore()
        if test in OPEN and test not in SOUND:
            SOUND[test] = survey(test, prefix)[1]
            print(f"{test}: seeds failing unchanged, counted only where they fail otherwise: {' '.join(sorted(SOUND[test], key=int))}", flush=True)
        seeds = [seed for seed, how in seeds.items() if SOUND.get(test, {}).get(seed) != how]
        print(f"{name}: caught by {len(seeds)} of {ran} seeds ({test}): {' '.join(seeds[:12])}", flush=True)
        if not seeds:
            escaped.append(name)
    if escaped:
        sys.exit(f"escaped the simulation: {' '.join(escaped)}")


main()
