use super::*;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

/// Items spread over three enclaves out of order, as a pool's players are.
fn items() -> Vec<(EnclaveId, u32)> {
    [2, 1, 3, 2, 1, 3, 2, 2]
        .into_iter()
        .zip(0..)
        .map(|(enclave, item)| (EnclaveId::new(enclave), item))
        .collect()
}

/// What each enclave was asked for, in the order it was asked, and how many shares ran at once.
#[derive(Default)]
struct Seen {
    asked: BTreeMap<u32, Vec<u32>>,
    running: usize,
    most_running: usize,
}

fn record_running_share(seen: &StdMutex<Seen>) {
    let mut seen = seen.lock().unwrap();
    seen.running += 1;
    seen.most_running = seen.most_running.max(seen.running);
}

/// Work on one enclave's share: the first enclave's items take longest, so shares finish out of
/// order. `fails` names an item that fails, after the items before it.
async fn work(
    seen: &StdMutex<Seen>,
    lane: Vec<u32>,
    items: Vec<u32>,
    fails: &[u32],
) -> Result<Vec<String>, KeymeldError> {
    assert!(lane.is_empty(), "each share starts from its own copy");
    record_running_share(seen);
    let mut lane = lane;
    let mut results = Vec::new();
    let mut failed = None;
    for item in items {
        let enclave = items_enclave(item);
        tokio::time::sleep(Duration::from_millis(u64::from(4 - enclave) * 5)).await;
        seen.lock()
            .unwrap()
            .asked
            .entry(enclave)
            .or_default()
            .push(item);
        if fails.contains(&item) {
            failed = Some(invalid(format!("item {item} failed")));
            break;
        }
        lane.push(item);
        results.push(format!("signed {item}"));
    }
    seen.lock().unwrap().running -= 1;
    match failed {
        Some(error) => Err(error),
        None => Ok(results),
    }
}

fn items_enclave(item: u32) -> u32 {
    items()[item as usize].0.as_u32()
}

#[tokio::test]
async fn per_enclave_results_match_the_serial_order() {
    let seen = StdMutex::new(Seen::default());
    let lane: Vec<u32> = Vec::new();
    let results = per_enclave(items(), &lane, |lane, items| work(&seen, lane, items, &[]))
        .await
        .unwrap();

    // The serial path: every item in turn.
    let serial: Vec<_> = items()
        .into_iter()
        .map(|(_, item)| format!("signed {item}"))
        .collect();
    assert_eq!(results, serial);
    let seen = seen.into_inner().unwrap();
    // Each enclave was asked for its items in their order.
    assert_eq!(
        seen.asked,
        BTreeMap::from([(1, vec![1, 4]), (2, vec![0, 3, 6, 7]), (3, vec![2, 5])])
    );
    assert_eq!(seen.most_running, 3, "the shares ran side by side");
    assert!(lane.is_empty());
}

#[tokio::test]
async fn per_enclave_fails_whole_when_one_share_fails() {
    let seen = StdMutex::new(Seen::default());
    let result = per_enclave(items(), &Vec::new(), |lane, items| {
        work(&seen, lane, items, &[3])
    })
    .await;
    let Err(error) = result else {
        panic!("a failed share failed the call");
    };
    assert!(error.to_string().contains("item 3 failed"), "{error}");
    // The other shares ran to their end rather than being dropped mid-request.
    let seen = seen.into_inner().unwrap();
    assert_eq!(seen.asked[&1], vec![1, 4]);
    assert_eq!(seen.asked[&3], vec![2, 5]);
    assert_eq!(seen.asked[&2], vec![0, 3]);

    // With two failures, the first enclave's is returned.
    let seen = StdMutex::new(Seen::default());
    let Err(error) = per_enclave(items(), &Vec::new(), |lane, items| {
        work(&seen, lane, items, &[5, 4])
    })
    .await
    else {
        panic!("failed shares failed the call");
    };
    assert!(error.to_string().contains("item 4 failed"), "{error}");
}

#[tokio::test]
async fn per_enclave_refuses_a_share_with_missing_results() {
    let result = per_enclave(items(), &(), |_, items: Vec<u32>| async move {
        Ok::<_, KeymeldError>(items.into_iter().skip(1).collect::<Vec<_>>())
    })
    .await;
    assert!(result.is_err());
}
