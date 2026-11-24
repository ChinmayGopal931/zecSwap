use super::{Head, Reveal, Stage, Window, Windows};

pub const READY_DEPTH: u64 = 2;
pub const REVERSE_READY_DEPTH: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Shut,
    MakersUntil(u64),
    TooSoon,
    ReadyMayLand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Go {
    Now,
    OnceReadyAt(u64),
}

pub fn forward_claim(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    wall: u64,
    margin: u64,
) -> Result<Go, Refusal> {
    let now = head.timestamp;
    let window = windows.window(Reveal::Claim, false, stage, (t0, t1), now);
    if window == Window::Shut {
        return Err(match stage {
            Stage::Open | Stage::Ready if now >= t1 => {
                Refusal::MakersUntil(t1.saturating_add(windows.late))
            }
            _ => Refusal::Shut,
        });
    }
    if !window.leaves(now.max(wall), margin) {
        return Err(Refusal::TooSoon);
    }
    Ok(if now < t0 {
        Go::OnceReadyAt(head.number.saturating_sub(READY_DEPTH))
    } else {
        Go::Now
    })
}

pub fn reverse_claim(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    wall: u64,
    margin: u64,
) -> Result<Go, Refusal> {
    let window = windows.window(Reveal::Claim, true, stage, (t0, t1), head.timestamp);
    if window == Window::Shut {
        return Err(Refusal::Shut);
    }
    if !window.leaves(head.timestamp.max(wall), margin) {
        return Err(Refusal::TooSoon);
    }
    Ok(Go::OnceReadyAt(
        head.number.saturating_sub(REVERSE_READY_DEPTH),
    ))
}

pub fn reverse_refund(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    ready_signed_until: Option<u64>,
    ready_may_land: bool,
) -> Result<(), Refusal> {
    let now = head.timestamp;
    if windows.window(Reveal::Refund, true, stage, (t0, t1), now) == Window::Shut {
        return Err(Refusal::Shut);
    }
    let signed =
        stage == Stage::Open && now < t0 && ready_signed_until.is_some_and(|until| now <= until);
    if signed || ready_may_land {
        return Err(Refusal::ReadyMayLand);
    }
    Ok(())
}

pub fn reverse_ready(stage: Stage, t0: u64, head: &Head, ttl: u64) -> Result<(), Refusal> {
    if stage != Stage::Open {
        return Err(Refusal::Shut);
    }
    if head.timestamp.saturating_add(ttl) >= t0 {
        return Err(Refusal::TooSoon);
    }
    Ok(())
}
