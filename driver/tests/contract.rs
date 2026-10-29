//! HYPERFORGE-9 · the contract, served and called: a scripted driver behind
//! `server()`, on a service, through the emitted client in process and over a
//! real WebSocket, as a proxy, and held as `dyn Driver`.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use hyperforge_driver::{
    codes, not_supported, Action, BoxStream, Decision, Driver, DriverActivation, DriverClient,
    DriverEvent, Inputs, LogSupport, RunId, RunState, Target, TargetName,
};
use plexus_core::plexus::{Activation, InProcessTransport, PlexusStreamItem};
use plexus_core::DynamicHub as DynamicService;
use serde_json::{json, Value};
use support::scripted::{target, Scripted};

fn kinds(events: &[DriverEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            DriverEvent::State { state, .. } => format!("state:{}", serde_json::to_value(state).unwrap().as_str().unwrap()),
            other => serde_json::to_value(other).unwrap()["type"].as_str().unwrap().to_string(),
        })
        .collect()
}

fn inputs(revision: &str) -> Inputs {
    BTreeMap::from([("revision".to_string(), revision.to_string())])
}

/// The emitted client, called through the trait (its inherent methods answer `ClientEvent`s).
fn through_the_client<D: Driver>(driver: D) -> Box<dyn Driver> {
    Box::new(DriverClient::new(InProcessTransport::new(driver.server())))
}

fn run_of(events: &[DriverEvent]) -> RunId {
    match &events[0] {
        DriverEvent::Run { id, .. } => id.clone(),
        other => panic!("trigger's first event is the run, got {other:?}"),
    }
}

#[test]
fn served_it_is_the_driver_namespace_with_six_methods() {
    let served: DriverActivation<Scripted> = Scripted::new().server();
    assert_eq!(served.namespace(), "driver");
    assert_eq!(served.methods(), vec!["describe", "trigger", "watch", "log", "cancel", "approve", "schema"]);
    assert!(served.description().starts_with("A deploy driver"), "{}", served.description());
}

#[test]
fn the_schema_carries_every_method_parameter_and_state() {
    let schema = serde_json::to_string(&Scripted::new().server().plugin_schema()).unwrap();
    for needle in [
        // methods and their doc comments
        "Start an action on a target", "Follow a run already started", "Ask a run to stop", "Answer a waiting run's gate",
        // parameters, with theirs
        "what to act on", "name → value, as `describe` names them", "the last line already had", "which gate, when it waits on more than one",
        // S1's types (HYPERFORGE-25)
        "rolled_back", "cancel_requested", "reject", "after", "live", "records", "gates", "seq", "boolean",
    ] {
        assert!(schema.contains(needle), "the schema lacks {needle:?}");
    }
}

async fn data(service: &DynamicService, method: &str, params: Value) -> Vec<Value> {
    service.route(method, params, None)
        .await
        .expect("routed")
        .filter_map(|item| async move {
            match item {
                PlexusStreamItem::Data { content, .. } => Some(content),
                PlexusStreamItem::Error { message, .. } => panic!("error item: {message}"),
                _ => None,
            }
        })
        .collect()
        .await
}

#[tokio::test]
async fn on_a_service_it_answers_by_its_wire_names() {
    let service = DynamicService::new("toy").register(Scripted::new().server());
    let described = data(&service, "driver.describe", json!({})).await;
    assert_eq!(described[0]["type"], "described");
    assert_eq!(described[0]["described"]["capabilities"], json!({"log": "live", "cancel": true, "approve": true, "records": false}));
    assert_eq!(described[0]["described"]["targets"][1], json!({"repo": {"org": "hypermemetic", "name": "menger"}, "name": "prod"}));

    let params = json!({
        "target": {"repo": {"org": "hypermemetic", "name": "menger"}, "name": "test"},
        "action": "deploy", "inputs": {"revision": "4c9b1bd"}, "reason": "the week's work",
    });
    let run = data(&service, "driver.trigger", params).await;
    let types: Vec<&str> = run.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["run", "state", "state", "log", "log", "state", "record"]);
    assert_eq!(run[0]["id"], "scripted-1");
    assert_eq!(run[3]["seq"], 1);
    assert_eq!(run[5]["state"], "succeeded");
    assert_eq!(run[6]["record"]["revision"], "4c9b1bd");
    assert_eq!(run[6]["record"]["environment"], "test");
}

#[tokio::test]
async fn a_bad_target_name_is_refused_before_the_driver() {
    assert!(TargetName::new("prod").is_ok());
    for bad in ["", ".hidden", "a/b", "ünï", &"x".repeat(65)] {
        assert!(TargetName::new(bad).is_err(), "{bad:?}");
        assert!(serde_json::from_value::<TargetName>(json!(bad)).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn through_the_client_a_run_goes_through_to_its_record() {
    let client = through_the_client(Scripted::new());
    let events: Vec<DriverEvent> = client.trigger(target("menger", "test"), Action::Deploy, inputs("4c9b1bd"), "why".into()).collect().await;
    assert_eq!(kinds(&events), ["run", "state:queued", "state:running", "log", "log", "state:succeeded", "record"]);

    // watch on an ended run: what it kept, then the end
    let watched: Vec<DriverEvent> = client.watch(run_of(&events)).collect().await;
    assert_eq!(kinds(&watched), ["state:queued", "state:running", "log", "log", "state:succeeded", "record"]);

    // log after the first line: the second alone
    let lines: Vec<DriverEvent> = client.log(run_of(&events), Some(1)).collect().await;
    assert!(matches!(&lines[..], [DriverEvent::Log { seq: 2, line, .. }] if line == "smoke passed"), "{lines:?}");
}

#[tokio::test]
async fn a_promote_waits_on_its_gate_until_approved() {
    let driver = Scripted::new();
    let client = through_the_client(driver.clone());
    let mut trigger = client.trigger(target("menger", "prod"), Action::Promote, Inputs::new(), "ship it".into());
    let mut seen = vec![];
    while let Some(e) = trigger.next().await {
        let waiting = matches!(&e, DriverEvent::State { state: RunState::Waiting, .. });
        seen.push(e);
        if waiting {
            break;
        }
    }
    let run = run_of(&seen);
    match seen.last() {
        Some(DriverEvent::State { gates, reason, .. }) => {
            assert_eq!(gates[0].name, "prod");
            assert_eq!(reason.as_deref(), Some("needs approval"));
        }
        other => panic!("{other:?}"),
    }

    let wrong: Vec<DriverEvent> = client.approve(run.clone(), Some("staging".into()), Decision::Approve, "owner".into(), None).collect().await;
    assert!(matches!(&wrong[..], [DriverEvent::Error { code, .. }] if code == codes::REFUSED), "{wrong:?}");

    let answer: Vec<DriverEvent> = client.approve(run.clone(), None, Decision::Approve, "owner".into(), Some("looks right".into())).collect().await;
    assert_eq!(kinds(&answer), ["state:running"]);

    // the trigger's stream goes on to the end
    let rest: Vec<DriverEvent> = trigger.collect().await;
    assert_eq!(kinds(&rest), ["state:running", "log", "state:succeeded", "record"]);
}

#[tokio::test]
async fn a_rejected_gate_fails_the_run() {
    let client = through_the_client(Scripted::new());
    let started: Vec<DriverEvent> = client.trigger(target("menger", "prod"), Action::Promote, Inputs::new(), "r".into()).take(5).collect().await;
    let answer: Vec<DriverEvent> = client.approve(run_of(&started), Some("prod".into()), Decision::Reject, "owner".into(), None).collect().await;
    assert!(matches!(&answer[..], [DriverEvent::State { state: RunState::Failed, reason: Some(r), .. }] if r == "rejected by owner"), "{answer:?}");
}

#[tokio::test]
async fn cancel_is_a_request_and_canceled_follows_on_the_run() {
    let client = through_the_client(Scripted::new());
    let started: Vec<DriverEvent> = client.trigger(target("menger", "prod"), Action::Promote, Inputs::new(), "r".into()).take(5).collect().await;
    let run = run_of(&started);
    let asked: Vec<DriverEvent> = client.cancel(run.clone()).collect().await;
    assert_eq!(kinds(&asked), ["cancel_requested"]);
    let watched: Vec<DriverEvent> = client.watch(run.clone()).collect().await;
    assert_eq!(kinds(&watched).last().map(String::as_str), Some("state:canceled"));
    let again: Vec<DriverEvent> = client.cancel(run).collect().await;
    assert!(matches!(&again[..], [DriverEvent::Error { code, .. }] if code == codes::REFUSED), "{again:?}");
}

#[tokio::test]
async fn an_unknown_run_or_target_is_said_in_band() {
    let client = through_the_client(Scripted::new());
    let got: Vec<DriverEvent> = client.watch(RunId::from("nope")).collect().await;
    assert!(matches!(&got[..], [DriverEvent::Error { code, .. }] if code == codes::UNKNOWN_RUN), "{got:?}");
    let got: Vec<DriverEvent> = client.trigger(target("plexus-core", "crates.io"), Action::Publish, Inputs::new(), "r".into()).collect().await;
    assert!(matches!(&got[..], [DriverEvent::Error { code, .. }] if code == codes::REFUSED), "{got:?}");
}

/// A driver that claims nothing but describe and trigger answers the rest not_supported, on the wire too.
struct Bare;
impl Driver for Bare {
    fn describe(&self) -> BoxStream<'static, DriverEvent> {
        Scripted::new().describe()
    }
    fn trigger(&self, _: Target, _: Action, _: Inputs, _: String) -> BoxStream<'static, DriverEvent> {
        not_supported("trigger")
    }
    fn watch(&self, _: RunId) -> BoxStream<'static, DriverEvent> {
        not_supported("watch")
    }
    fn log(&self, _: RunId, _: Option<u64>) -> BoxStream<'static, DriverEvent> {
        not_supported("log")
    }
    fn cancel(&self, _: RunId) -> BoxStream<'static, DriverEvent> {
        not_supported("cancel")
    }
    fn approve(&self, _: RunId, _: Option<String>, _: Decision, _: String, _: Option<String>) -> BoxStream<'static, DriverEvent> {
        not_supported("approve")
    }
}

#[tokio::test]
async fn what_a_driver_lacks_is_not_supported() {
    let client = through_the_client(Bare);
    let got: Vec<DriverEvent> = client.cancel(RunId::from("1")).collect().await;
    assert_eq!(got, vec![DriverEvent::error(codes::NOT_SUPPORTED, "this driver does not support cancel")]);
}

#[tokio::test]
async fn a_client_served_again_is_a_proxy_to_the_driver() {
    let remote = InProcessTransport::new(Scripted::new().server());
    let service = DynamicService::new("toy").register(DriverClient::new(remote).server());
    let described = data(&service, "driver.describe", json!({})).await;
    assert_eq!(described[0]["described"]["capabilities"]["log"], "live");
}

#[tokio::test]
async fn built_in_served_and_remote_are_one_type_to_hyperforge() {
    let served = Scripted::new().server();
    let drivers: BTreeMap<&str, Arc<dyn Driver>> = BTreeMap::from([
        ("built-in", Arc::new(Scripted::new()) as Arc<dyn Driver>),
        ("served", Arc::new(served.clone())),
        ("plugin", Arc::new(DriverClient::new(InProcessTransport::new(served)))),
    ]);
    for (name, d) in drivers {
        let got: Vec<DriverEvent> = d.describe().collect().await;
        assert!(matches!(&got[..], [DriverEvent::Described { described }] if described.capabilities.log == LogSupport::Live), "{name}: {got:?}");
    }
}

#[tokio::test]
async fn over_a_websocket_the_client_drives_a_run() {
    use plexus_client::WsTransport;
    use plexus_transport::TransportServer;

    let service = Arc::new(DynamicService::new("toy").register(Scripted::new().server()));
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let server = TransportServer::builder(service, |h| DynamicService::arc_into_rpc_module(h).map_err(Into::into))
        .with_websocket(port)
        .without_registry()
        .build()
        .await
        .expect("the toy service builds");
    tokio::spawn(async move { server.serve().await });

    let mut transport = None;
    for _ in 0..50 {
        match WsTransport::connect(&format!("ws://127.0.0.1:{port}"), "toy").await {
            Ok(t) => {
                transport = Some(t);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    let client: Box<dyn Driver> = Box::new(DriverClient::new(transport.expect("connected")));
    let events: Vec<DriverEvent> = client.trigger(target("menger", "test"), Action::Deploy, inputs("cc95031"), "over the wire".into()).collect().await;
    assert_eq!(kinds(&events), ["run", "state:queued", "state:running", "log", "log", "state:succeeded", "record"]);
}
