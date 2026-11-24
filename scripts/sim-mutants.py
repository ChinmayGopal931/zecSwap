#!/usr/bin/env python3
import os
import pathlib
import shutil
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
BACKUP = ROOT / "target" / "sim-mutants"
FORWARD = ("survey", "seed ")
BACKWARD = ("reverse_survey", "reverse seed ")
DEPOSIT = ("deposit_survey", "deposit seed ")

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
    return len(lines), [line.split(":")[0].removeprefix(prefix) for line in failing]


def restore():
    for saved in sorted(BACKUP.glob("*")):
        restored = ROOT / saved.name.replace("__", "/")
        shutil.move(saved, restored)
        os.utime(restored)


def main():
    BACKUP.mkdir(parents=True, exist_ok=True)
    if any(BACKUP.iterdir()):
        restore()
        print("undid the mutant an earlier run left in place", flush=True)
    escaped = []
    for name, ((test, prefix), edits) in MUTANTS.items():
        if ONLY and name not in ONLY:
            continue
        try:
            for path in sorted({path for path, _, _ in edits}):
                shutil.copy(ROOT / path, BACKUP / path.replace("/", "__"))
            for path, old, new in edits:
                text = (ROOT / path).read_text()
                if text.count(old) != 1:
                    sys.exit(f"{name}: {path} no longer has the line this mutant changes")
                (ROOT / path).write_text(text.replace(old, new))
            ran, seeds = survey(test, prefix)
        finally:
            restore()
        print(f"{name}: caught by {len(seeds)} of {ran} seeds ({test}): {' '.join(seeds[:12])}", flush=True)
        if not seeds:
            escaped.append(name)
    if escaped:
        sys.exit(f"escaped the simulation: {' '.join(escaped)}")


main()
