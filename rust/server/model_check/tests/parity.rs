//! Model-check admission cases beyond the ones beside the budget: a remembered pass and a shared run cost
//! nothing against the device and provider limits.

use serde_json::json;

use super::super::budget::{Admission, CheckBudget};

fn started(admission: Admission) -> tokio::sync::watch::Receiver<Option<serde_json::Value>> {
    match admission {
        Admission::Start(receiver) => receiver,
        _ => panic!("expected the check to start"),
    }
}

#[test]
fn twenty_identical_passed_checks_cost_one_run() {
    let budget = CheckBudget::default();
    started(budget.admit("same", "a", "openai"));
    budget.complete("same", json!({"ok": true, "passes": []}));
    for _ in 0..20 {
        let Admission::Cached(answer) = budget.admit("same", "a", "openai") else {
            panic!("a passed check answers for itself");
        };
        assert_eq!(answer["remembered"], true);
    }
    for n in 0..5 {
        started(budget.admit(&format!("other-{n}"), "a", "openai"));
    }
    let Admission::Limited(_, scope) = budget.admit("other-5", "a", "openai") else {
        panic!("the sixth run of the minute is limited");
    };
    assert_eq!(scope, "device", "only the one real run counted");
}

#[tokio::test]
async fn identical_checks_asked_together_share_one_run() {
    let budget = CheckBudget::default();
    let mut first = started(budget.admit("same", "0", "openai"));
    let mut joined = Vec::new();
    for device in 1..8 {
        let Admission::Join(receiver) = budget.admit("same", &device.to_string(), "openai") else {
            panic!("an identical check in flight is joined");
        };
        joined.push(receiver);
    }
    budget.complete("same", json!({"ok": true, "passes": []}));
    first.changed().await.unwrap();
    assert_eq!(first.borrow().as_ref().unwrap()["ok"], true);
    for mut receiver in joined {
        receiver.changed().await.unwrap();
        assert_eq!(receiver.borrow().as_ref().unwrap()["ok"], true);
    }
    // Joining cost the joiners nothing: each still has its whole minute.
    for n in 0..6 {
        started(budget.admit(&format!("own-{n}"), "1", "elevenlabs"));
    }
}
