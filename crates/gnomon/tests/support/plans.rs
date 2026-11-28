//! The query plans of one Store call (#124), nested statements included.
//!
//! The call runs once through the recording proxy of [`wire`], which keeps
//! what the Store sent for it: the caller's role, the claims, and each
//! statement with its parameters. All of it is then sent again on a superuser
//! connection that has loaded `auto_explain`, which only a superuser can.
//! Row-level security applies as it did to the call, because the transaction
//! takes the caller's role.
//!
//! The first five replays are unlogged, as many as the executions PL/pgSQL
//! plans for their own parameters. In the sixth, the logged one, it can keep
//! a generic plan, as it can on a connection that has served calls. The
//! replays commit, so only a call that reads may be explained.

use std::error::Error;

use bytes::BytesMut;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{AsyncMessage, Client, NoTls};

use super::Server;
use super::wire::{self, Replayable};

/// Replays before the logged one: as many as the executions PL/pgSQL plans
/// for their parameters before it may keep a generic plan.
const UNLOGGED_REPLAYS: usize = 5;

/// `auto_explain` for the session, reporting to the client. It logs nothing
/// until `auto_explain.log_min_duration` is set. The parameters are cut
/// short, because a query embedding would fill the plan.
const AUTO_EXPLAIN: &str = "
load 'auto_explain';
set lc_messages = 'C';
set client_min_messages = notice;
set auto_explain.log_level = notice;
set auto_explain.log_format = json;
set auto_explain.log_analyze = on;
set auto_explain.log_buffers = on;
set auto_explain.log_nested_statements = on;
set auto_explain.log_parameter_max_length = 64;
";

/// The plans of the one Store call that `call` makes on database `db`, in the
/// order the statements ended, so the Store's own statement comes after those
/// nested in it. Each is `{"duration_ms", "explain"}`, `explain` being
/// `auto_explain`'s JSON. The durations include the cost of measuring each
/// plan node, so they compare with no timed call.
pub async fn of_call(db: &str, call: impl AsyncFnOnce(&gnomon::Store)) -> Vec<Value> {
    let server = Server::get().await;
    let (host, port) = server.admin_host_port();
    let (proxy, log) = wire::record(super::resolve(&host, port).await).await;
    let url = super::login_url(super::LOGIN_ROLE, &proxy.ip().to_string(), proxy.port(), db);
    let store = super::connect_and_check(&url, &super::connect_options())
        .await
        .expect("the Store connects through the proxy");
    let before = log.len();
    call(&store).await;
    let sent = log.replayable_since(before);

    let (client, mut connection) = server
        .admin_config(db)
        .connect(NoTls)
        .await
        .expect("connect to the test server");
    let (notices, mut received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // The connection reports each notice before it passes on the end of
        // the statement that raised it.
        while let Some(Ok(message)) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
            if let AsyncMessage::Notice(notice) = message {
                let _ = notices.send(notice.message().to_owned());
            }
        }
    });
    client
        .batch_execute(AUTO_EXPLAIN)
        .await
        .expect("load auto_explain");
    for _ in 0..UNLOGGED_REPLAYS {
        for statement in &sent {
            send(&client, statement).await;
        }
    }
    client
        .batch_execute("set auto_explain.log_min_duration = 0")
        .await
        .unwrap();
    // The first extended statement of the call's transaction sets its claims.
    let (claims, claims_sql) = sent
        .iter()
        .enumerate()
        .find_map(|(i, s)| match s {
            Replayable::Extended { sql, .. } => Some((i, sql)),
            Replayable::Simple(_) => None,
        })
        .expect("the call sends its claims");
    let (setup, calls) = sent.split_at(claims + 1);
    for statement in setup {
        send(&client, statement).await;
    }
    while received.try_recv().is_ok() {}
    for statement in calls {
        send(&client, statement).await;
    }
    // A statement ends, and its plan is reported, when the next one takes its
    // place or the transaction ends. So the claims' own plan is among these.
    let mut plans = vec![];
    while let Ok(notice) = received.try_recv() {
        plans.extend(plan(&notice).filter(|p| p["explain"]["Query Text"] != claims_sql.as_str()));
    }
    assert!(!plans.is_empty(), "auto_explain reported no plan");
    plans
}

/// The plan a notice of `auto_explain` holds, if it is one.
fn plan(notice: &str) -> Option<Value> {
    let (head, explain) = notice.split_once("plan:\n")?;
    let ms: f64 = head
        .trim()
        .strip_prefix("duration: ")?
        .strip_suffix(" ms")?
        .parse()
        .ok()?;
    let explain: Value = serde_json::from_str(explain).expect("auto_explain writes JSON");
    Some(json!({ "duration_ms": ms, "explain": explain }))
}

async fn send(client: &Client, statement: &Replayable) {
    match statement {
        Replayable::Simple(sql) => client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("replay {sql}: {e:?}")),
        Replayable::Extended { sql, types, params } => {
            let params: Vec<(Binary, Type)> = params
                .iter()
                .zip(types)
                .map(|(p, oid)| {
                    let kind = Type::from_oid(*oid).expect("a built-in type");
                    (Binary(p.clone()), kind)
                })
                .collect();
            let params: Vec<(&(dyn ToSql + Sync), Type)> = params
                .iter()
                .map(|(p, kind)| (p as &(dyn ToSql + Sync), kind.clone()))
                .collect();
            client
                .query_typed(sql, &params)
                .await
                .unwrap_or_else(|e| panic!("replay {sql}: {e:?}"));
        }
    }
}

/// A parameter as the client encoded it, in the binary format.
#[derive(Debug)]
struct Binary(Option<Vec<u8>>);

impl ToSql for Binary {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match &self.0 {
            Some(bytes) => {
                out.extend_from_slice(bytes);
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}
