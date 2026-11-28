use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

use tokio::task::{JoinError, JoinHandle};

const TOLD: usize = 3;

pub(crate) struct Killed;

pub(crate) struct Unsent;

thread_local! {
    static HELD: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn hold(what: String) {
    let _ = HELD.try_with(|held| {
        let mut held = held.borrow_mut();
        if held.len() < TOLD && !held.contains(&what) {
            held.push(what);
        }
    });
}

pub(crate) fn caught<T>(simulation: impl FnOnce() -> T) -> (Option<T>, Vec<String>) {
    static HOOKED: Once = Once::new();
    HOOKED.call_once(|| {
        let usual = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if info.payload().is::<Killed>() || info.payload().is::<Unsent>() {
                return;
            }
            hold(
                info.to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            usual(info);
        }));
    });
    HELD.take();
    let out = panic::catch_unwind(AssertUnwindSafe(simulation)).ok();
    let mut panics = HELD.take();
    if out.is_none() && panics.is_empty() {
        panics.push("the simulation panicked".into());
    }
    (out, panics)
}

pub(crate) fn judge<T>(who: &str, ended: Result<T, JoinError>) -> Option<T> {
    let panicked = ended.as_ref().is_err_and(JoinError::is_panic);
    if panicked && HELD.with_borrow(Vec::is_empty) {
        hold(format!("{who} panicked"));
    }
    ended.ok()
}

pub(crate) type Named = (&'static str, JoinHandle<()>);

pub(crate) async fn end(tasks: impl IntoIterator<Item = Named>) {
    let tasks: Vec<Named> = tasks.into_iter().collect();
    for (_, task) in &tasks {
        task.abort();
    }
    for (who, task) in tasks {
        judge(who, task.await);
    }
}
