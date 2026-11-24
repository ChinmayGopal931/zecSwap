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
BACKUP = ROOT / "target" / "sim-mutants"

MUTANTS = {
    "ready-depth-0": [
        (JUDGE, "pub const READY_DEPTH: u64 = 2;", "pub const READY_DEPTH: u64 = 0;"),
    ],
    "no-claim-margin": [
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
    ],
    "no-deadline-check": [
        (
            USER,
            "        check_deadlines(now, self.min_time_to_t0, chain.t0, chain.t1)?;\n",
            "        let _ = now;\n",
        ),
    ],
    "any-terms": [
        (EVM, "    if state.terms_hash != terms.hash() {", "    if false {"),
    ],
    "any-open-fee": [
        (USER, "        fee.zat <= max,\n", "        fee.zat <= max || true,\n"),
    ],
    "claim-any-time": [
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
    ],
    "claim-in-the-makers-hour": [
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
    ],
}


def survey():
    run = subprocess.run(
        ["cargo", "test", "-p", "zecswap-sim", "--test", "swaps", "survey", "--", "--ignored", "--nocapture"],
        cwd=ROOT,
        env={**os.environ, "ZECSWAP_SIM_SEEDS": SEEDS},
        capture_output=True,
        text=True,
    )
    lines = [line for line in run.stdout.splitlines() if line.startswith("seed ")]
    if not lines:
        sys.exit(run.stderr[-3000:])
    return [line for line in lines if not line.rstrip().endswith("|")]


def restore():
    for saved in sorted(BACKUP.glob("*")):
        shutil.move(saved, ROOT / saved.name.replace("__", "/"))


def main():
    BACKUP.mkdir(parents=True, exist_ok=True)
    if any(BACKUP.iterdir()):
        restore()
        print("undid the mutant an earlier run left in place", flush=True)
    escaped = []
    for name, edits in MUTANTS.items():
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
            failing = survey()
        finally:
            restore()
        seeds = [line.split(":")[0].removeprefix("seed ") for line in failing]
        print(f"{name}: caught by {len(seeds)} seeds {' '.join(seeds[:12])}", flush=True)
        if not seeds:
            escaped.append(name)
    if escaped:
        sys.exit(f"escaped the simulation: {' '.join(escaped)}")


main()
