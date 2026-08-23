use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::ids::TurnId;

type TurnPanicScripts = Mutex<HashSet<TurnId>>;

pub(in crate::agent::runner) fn next_scripted_turn_id() -> TurnId {
    static NEXT: AtomicU64 = AtomicU64::new(900);
    let value = NEXT.fetch_add(1, Ordering::SeqCst);
    format!("trn_{value:032}").parse().unwrap()
}

pub(in crate::agent::runner) fn script_turn_panic(turn_id: TurnId) {
    turn_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(turn_id);
}

pub(in crate::agent::runner) fn take_scripted_turn_panic(turn_id: TurnId) -> bool {
    turn_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&turn_id)
}

fn turn_panic_scripts() -> &'static TurnPanicScripts {
    static SCRIPTS: OnceLock<TurnPanicScripts> = OnceLock::new();
    SCRIPTS.get_or_init(|| Mutex::new(HashSet::new()))
}
