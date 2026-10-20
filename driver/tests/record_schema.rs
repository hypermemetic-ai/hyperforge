//! HYPERFORGE-9 · `Record` is DEPLOY-10's record field for field: the
//! vendored `fixtures/record.schema.json` (menger's `deploy/record.schema.json`)
//! and the Rust type agree on every property, the required ones, the states,
//! and the schema's own examples; and the vendored copy is menger's, when
//! menger is checked out beside hyperforge.

use std::collections::BTreeSet;

use hyperforge_driver::{Record, RecordSource, RecordState};
use serde_json::{json, Value};

const VENDORED: &str = include_str!("fixtures/record.schema.json");

fn schema() -> Value {
    serde_json::from_str(VENDORED).unwrap()
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn full() -> Record {
    Record {
        project: "menger".into(),
        environment: "prod".into(),
        state: RecordState::RolledBack,
        revision: "235ff62".into(),
        previous: Some("29a50be".into()),
        commits: vec!["824e943".into()],
        tickets: vec!["ENV-2".into()],
        release: Some("20261012T220402Z-235ff62".into()),
        at: Some("2026-10-12T22:04:19Z".into()),
        from: Some("test".into()),
        request: Some(7),
        source: Some(RecordSource { system: "hyperforge".into(), run: Some("41".into()), url: Some("https://x.invalid/41".into()) }),
    }
}

#[test]
fn every_property_and_no_other() {
    let s = schema();
    let full = serde_json::to_value(full()).unwrap();
    assert_eq!(keys(&full), keys(&s["properties"]));
    assert_eq!(keys(&full["source"]), keys(&s["properties"]["source"]["properties"]));
}

#[test]
fn the_required_ones_are_the_ones_always_written() {
    let s = schema();
    let bare = Record { previous: None, commits: vec![], tickets: vec![], release: None, at: None, from: None, request: None, source: None, ..full() };
    let required: BTreeSet<String> = s["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(keys(&serde_json::to_value(bare).unwrap()), required);
    let source_required: Vec<&str> = s["properties"]["source"]["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(source_required, ["system"]);
}

#[test]
fn the_states_are_deploy_10s() {
    let states: Vec<Value> = [RecordState::Started, RecordState::Succeeded, RecordState::Failed, RecordState::RolledBack]
        .iter()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect();
    assert_eq!(Value::Array(states), schema()["properties"]["state"]["enum"]);
}

fn without_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, without_nulls(v))).collect()),
        other => other,
    }
}

#[test]
fn the_schemas_examples_read_and_write_back() {
    for example in schema()["examples"].as_array().unwrap() {
        let record: Record = serde_json::from_value(example.clone()).unwrap();
        assert_eq!(serde_json::to_value(&record).unwrap(), without_nulls(example.clone()));
    }
    assert!(serde_json::from_value::<Record>(json!({"project": "p", "environment": "e", "state": "started", "revision": "abcdef0", "extra": 1})).is_err());
}

#[test]
fn the_vendored_schema_is_mengers() {
    let mengers = concat!(env!("CARGO_MANIFEST_DIR"), "/../../menger/deploy/record.schema.json");
    match std::fs::read_to_string(mengers) {
        Ok(live) => assert_eq!(serde_json::from_str::<Value>(&live).unwrap(), schema(), "menger's record schema moved: copy it to tests/fixtures and match Record to it"),
        Err(_) => eprintln!("menger is not beside hyperforge; the vendored schema stands"),
    }
}
