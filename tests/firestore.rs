//! The Firestore client against the emulator. `just test` (or
//! `scripts/with-firestore-emulator.sh cargo test`) starts one; without
//! `FIRESTORE_EMULATOR_HOST` every test here skips.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use rustygcp::firestore::{integer_value, string_value};
use rustygcp::{Doc, Error, Firestore, Precondition, Write};
use serde_json::{Map, Value, json};

/// A client on a fresh emulator project (a project is a namespace there), or
/// `None` when no emulator is running.
fn firestore(test: &str) -> Option<Firestore> {
    static N: AtomicU32 = AtomicU32::new(0);
    let Ok(host) = std::env::var("FIRESTORE_EMULATOR_HOST") else {
        eprintln!("{test}: skipped, FIRESTORE_EMULATOR_HOST unset (run `just test`)");
        return None;
    };
    let project = format!("fake-{}-{}", std::process::id(), N.fetch_add(1, Relaxed));
    Some(Firestore::emulator(&host, &project))
}

fn fields(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn put(path: &str, v: Value, pre: Precondition) -> Write {
    Write::Put {
        path: path.into(),
        fields: fields(v),
        pre,
    }
}

async fn must_get(fs: &Firestore, path: &str) -> Doc {
    fs.get(path)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{path} is missing"))
}

#[tokio::test]
async fn put_get_delete() {
    let Some(fs) = firestore("put_get_delete") else {
        return;
    };
    assert!(fs.get("games/g1").await.unwrap().is_none());

    let doc = json!({ "name": string_value("first"), "phase": integer_value(2) });
    let times = fs
        .commit(&[
            put("games/g1", doc, Precondition::None),
            put("games/g2", json!({}), Precondition::None),
        ])
        .await
        .unwrap();
    assert_eq!(times.len(), 2);

    let got = must_get(&fs, "games/g1").await;
    assert_eq!(
        (got.string("name"), got.integer("phase")),
        (Some("first"), Some(2))
    );
    assert_eq!(
        got.update_time, times[0],
        "commit returns the update_time a later get shows"
    );

    // Put replaces the whole document
    fs.commit(&[put(
        "games/g1",
        json!({ "phase": integer_value(3) }),
        Precondition::None,
    )])
    .await
    .unwrap();
    let got = must_get(&fs, "games/g1").await;
    assert_eq!((got.string("name"), got.integer("phase")), (None, Some(3)));

    fs.commit(&[Write::Delete {
        path: "games/g1".into(),
    }])
    .await
    .unwrap();
    assert!(fs.get("games/g1").await.unwrap().is_none());
    assert!(fs.get("games/g2").await.unwrap().is_some());
}

#[tokio::test]
async fn preconditions_refuse_and_write_nothing() {
    let Some(fs) = firestore("preconditions") else {
        return;
    };
    let v = |n| json!({ "n": integer_value(n) });

    // create-if-absent
    fs.commit(&[put("c/d", v(1), Precondition::Exists(false))])
        .await
        .unwrap();
    let again = fs
        .commit(&[put("c/d", v(2), Precondition::Exists(false))])
        .await;
    assert!(matches!(again, Err(Error::Precondition)), "{again:?}");

    // must-exist
    let absent = fs
        .commit(&[put("c/absent", v(1), Precondition::Exists(true))])
        .await;
    assert!(matches!(absent, Err(Error::Precondition)), "{absent:?}");
    assert!(fs.get("c/absent").await.unwrap().is_none());

    // read-modify-write on update_time: the first writer wins
    let read = must_get(&fs, "c/d").await;
    let pre = || Precondition::UpdateTime(read.update_time.clone());
    fs.commit(&[put("c/d", v(10), pre())]).await.unwrap();
    let stale = fs.commit(&[put("c/d", v(20), pre())]).await;
    assert!(matches!(stale, Err(Error::Precondition)), "{stale:?}");
    assert_eq!(must_get(&fs, "c/d").await.integer("n"), Some(10));

    // a commit is atomic: one failed precondition and none of it lands
    let half = fs
        .commit(&[
            put("c/other", v(1), Precondition::None),
            Write::Increment {
                path: "c/counter".into(),
                by: vec![("n".into(), 1)],
            },
            put("c/d", v(30), pre()),
        ])
        .await;
    assert!(matches!(half, Err(Error::Precondition)), "{half:?}");
    assert!(fs.get("c/other").await.unwrap().is_none());
    assert!(fs.get("c/counter").await.unwrap().is_none());
    assert_eq!(must_get(&fs, "c/d").await.integer("n"), Some(10));
}

#[tokio::test]
async fn increment_creates_and_adds() {
    let Some(fs) = firestore("increment") else {
        return;
    };
    let inc = |by: &[(&str, i64)]| Write::Increment {
        path: "spend/2026-10-03".into(),
        by: by.iter().map(|(f, n)| (f.to_string(), *n)).collect(),
    };
    // field names that are not identifiers go through quoting
    fs.commit(&[inc(&[
        ("micros", 5),
        ("a|b", 1),
        ("with`tick", 1),
        ("back\\slash", 1),
    ])])
    .await
    .unwrap();
    fs.commit(&[inc(&[("micros", 7), ("a|b", -3)])])
        .await
        .unwrap();

    let mut got: Vec<_> = must_get(&fs, "spend/2026-10-03")
        .await
        .integers()
        .map(|(k, n)| (k.to_string(), n))
        .collect();
    got.sort();
    let want = [
        ("a|b", -2),
        ("back\\slash", 1),
        ("micros", 12),
        ("with`tick", 1),
    ];
    assert_eq!(got, want.map(|(k, n)| (k.to_string(), n)));

    // an increment leaves the other fields of an existing document alone
    fs.commit(&[put(
        "c/d",
        json!({ "s": string_value("keep") }),
        Precondition::None,
    )])
    .await
    .unwrap();
    fs.commit(&[Write::Increment {
        path: "c/d".into(),
        by: vec![("n".into(), 4)],
    }])
    .await
    .unwrap();
    let got = must_get(&fs, "c/d").await;
    assert_eq!((got.string("s"), got.integer("n")), (Some("keep"), Some(4)));
}

#[tokio::test]
async fn merge_sets_its_fields_and_keeps_the_rest() {
    let Some(fs) = firestore("merge") else { return };
    let merge = |v: Value, pre| Write::Merge {
        path: "t/shard-0".into(),
        fields: fields(v),
        pre,
    };
    // creates the document
    fs.commit(&[merge(
        json!({ "hello|ru": string_value("привет") }),
        Precondition::None,
    )])
    .await
    .unwrap();
    fs.commit(&[merge(
        json!({ "bye|ru": string_value("пока"), "odd`key": string_value("x") }),
        Precondition::Exists(true),
    )])
    .await
    .unwrap();
    fs.commit(&[merge(
        json!({ "bye|ru": string_value("до свидания") }),
        Precondition::None,
    )])
    .await
    .unwrap();

    let got = must_get(&fs, "t/shard-0").await;
    assert_eq!(got.fields.len(), 3);
    assert_eq!(got.string("hello|ru"), Some("привет"));
    assert_eq!(got.string("bye|ru"), Some("до свидания"));
    assert_eq!(got.string("odd`key"), Some("x"));
}

#[tokio::test]
async fn list_pages_and_query_filters() {
    let Some(fs) = firestore("list_and_query") else {
        return;
    };
    assert!(fs.list("items").await.unwrap().is_empty());
    assert!(
        fs.query("items", None, true, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fs.collection_ids().await.unwrap().is_empty());

    // 301 documents: one more than a list page
    let writes: Vec<Write> = (0..301)
        .map(|i| {
            let kind = if i % 100 == 0 { "rare" } else { "common" };
            let doc = json!({ "kind": string_value(kind), "i": integer_value(i) });
            put(&format!("items/i{i:03}"), doc, Precondition::None)
        })
        .chain([put("other/x", json!({}), Precondition::None)])
        .collect();
    fs.commit(&writes).await.unwrap();

    let all = fs.list("items").await.unwrap();
    assert_eq!(all.len(), 301);
    assert_eq!(all[0].0, "i000");
    assert_eq!(all[300].1.integer("i"), Some(300));
    assert!(!all[300].1.update_time.is_empty());

    let rare = json!({ "fieldFilter": {
        "field": { "fieldPath": "kind" }, "op": "EQUAL", "value": string_value("rare"),
    }});
    let ids = |docs: Vec<(String, Doc)>| docs.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    let got = fs
        .query("items", Some(rare.clone()), true, None)
        .await
        .unwrap();
    assert_eq!(ids(got), ["i000", "i100", "i200", "i300"]);
    let got = fs.query("items", Some(rare), true, Some(2)).await.unwrap();
    assert_eq!(ids(got), ["i000", "i100"]);
    let got = fs.query("items", None, true, Some(3)).await.unwrap();
    assert_eq!(ids(got), ["i000", "i001", "i002"]);

    // a document found by reference to itself
    let by_name = json!({ "fieldFilter": {
        "field": { "fieldPath": "__name__" }, "op": "EQUAL", "value": fs.reference("items/i042"),
    }});
    let got = fs.query("items", Some(by_name), false, None).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.integer("i"), Some(42));

    let mut collections = fs.collection_ids().await.unwrap();
    collections.sort();
    assert_eq!(collections, ["items", "other"]);
}

#[tokio::test]
async fn operations_are_counted_as_firestore_bills_them() {
    let Some(fs) = firestore("op_counts") else {
        return;
    };
    assert_eq!(fs.ops.snapshot(), (0, 0, 0));

    fs.get("c/absent").await.unwrap(); // a miss is a read
    fs.list("c").await.unwrap(); // an empty list is one read
    fs.query("c", None, false, None).await.unwrap(); // an empty query is one read
    assert_eq!(fs.ops.snapshot(), (3, 0, 0));

    let writes: Vec<Write> = (0..5)
        .map(|i| put(&format!("c/d{i}"), json!({}), Precondition::None))
        .collect();
    fs.commit(&writes).await.unwrap();
    fs.commit(&[
        Write::Increment {
            path: "c/n".into(),
            by: vec![("n".into(), 1)],
        },
        Write::Merge {
            path: "c/d0".into(),
            fields: fields(json!({ "s": string_value("x") })),
            pre: Precondition::None,
        },
        Write::Delete {
            path: "c/d1".into(),
        },
        Write::Delete {
            path: "c/d2".into(),
        },
    ])
    .await
    .unwrap();
    assert_eq!(fs.ops.snapshot(), (3, 7, 2));

    assert_eq!(fs.list("c").await.unwrap().len(), 4); // d0 d3 d4 n
    assert_eq!(fs.query("c", None, false, Some(2)).await.unwrap().len(), 2);
    fs.get("c/d0").await.unwrap();
    assert_eq!(fs.ops.snapshot(), (3 + 4 + 2 + 1, 7, 2));
}
