//! The query plans performance work reads (#124): those the performance suite
//! saves in its records. They run here, in the ordinary suite, because CI does
//! not run the performance suite.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use gnomon::{
    AsOf, Mention, NewEpisode, NewNode, Patch, ROOT_NODE_KIND, Scope, SearchRequest, Store,
    TenantId, ValidTime,
};
use serde_json::{Value, json};
use support::perf::Record;
use support::{TestDb, plans};

const SCOPE: &str = "here";

/// A database holding one Node named `alpha` in [`SCOPE`].
async fn one_node() -> (TestDb, TenantId) {
    let db = TestDb::migrated().await;
    let tenant = support::tenant();
    let store = db.store().await;
    let caller = store.as_worker(tenant, Scope::new(SCOPE), None);
    let episode = caller
        .record_episode(&NewEpisode {
            tenant: Some(tenant),
            scope: Scope::new(SCOPE),
            content: "an Episode".into(),
            reference_time: support::utc(2024, 1, 1),
            authority: None,
        })
        .await
        .unwrap();
    let Value::Object(values) = json!({ "name": "alpha" }) else {
        unreachable!()
    };
    caller
        .create_node(
            &NewNode {
                tenant: Some(tenant),
                scope: Scope::new(SCOPE),
                kind: ROOT_NODE_KIND.to_owned(),
            },
            &Patch::new(episode, ValidTime::since(support::utc(2024, 1, 1)), values),
        )
        .await
        .unwrap();
    (db, tenant)
}

/// One Search for `alpha` by a worker of `scope`.
async fn search_alpha(store: &Store, tenant: TenantId, scope: &str) {
    let request = SearchRequest::text("alpha", AsOf::current(support::utc(2025, 1, 1)));
    store
        .as_worker(tenant, Scope::new(scope), None)
        .search(tenant, &request, 10)
        .await
        .unwrap();
}

/// The plans of [`search_alpha`].
async fn search_plans(db: &TestDb, tenant: TenantId, scope: &str) -> Vec<Value> {
    plans::of_call(&db.name, async |store| {
        search_alpha(store, tenant, scope).await
    })
    .await
}

/// Whether any node of any of these plans reads `table`.
fn reads(plans: &[Value], table: &str) -> bool {
    fn in_node(node: &Value, table: &str) -> bool {
        node["Relation Name"] == table
            || node["Plans"]
                .as_array()
                .is_some_and(|children| children.iter().any(|c| in_node(c, table)))
    }
    plans.iter().any(|p| in_node(&p["explain"]["Plan"], table))
}

/// The plan of the statement the Store itself sent, whose text names
/// `function`. auto_explain reports it after the statements nested in it.
fn top_level_plan<'a>(plans: &'a [Value], function: &str) -> &'a Value {
    let last = plans.last().expect("a plan");
    let text = last["explain"]["Query Text"].as_str().unwrap();
    assert!(text.contains(function), "the last plan is of {text}");
    &last["explain"]
}

#[tokio::test]
async fn a_searchs_plans_include_its_nested_statements_as_the_caller() {
    let (db, tenant) = one_node().await;

    let plans = search_plans(&db, tenant, SCOPE).await;
    // The Store's statement scans one function. Only a nested statement shows
    // the token table that function reads.
    assert!(reads(&plans, "search_token"), "{plans:#?}");
    let own = top_level_plan(&plans, "gnomon.search(");
    assert_eq!(own["Plan"]["Actual Rows"].as_f64(), Some(1.0), "{own:#}");

    // Row-level security applies under the caller's role and claims, so a
    // worker of another Scope finds nothing.
    let elsewhere = search_plans(&db, tenant, "elsewhere").await;
    let own = top_level_plan(&elsewhere, "gnomon.search(");
    assert_eq!(own["Plan"]["Actual Rows"].as_f64(), Some(0.0), "{own:#}");
}

#[tokio::test]
async fn a_record_saves_the_plans_of_a_search_and_of_a_resolution_by_class() {
    let (db, tenant) = one_node().await;
    // A name of its own, to find this record among those on disk.
    let case = format!("query_plans_{}", db.name);
    let record = Record::start(&case, &db.name).await;
    let mut written = record.finish(json!({ "numbers": 1 }));

    // The numbers are on disk before a plan is taken.
    let read = |path: &Path| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    assert_eq!(read(&written.path)["results"], json!({ "numbers": 1 }));

    written
        .take_plans("a Search class", "alpha", async |store| {
            search_alpha(store, tenant, SCOPE).await
        })
        .await;
    written
        .take_plans("a Resolution class", "alpha", async |store| {
            store
                .as_worker(tenant, Scope::new(SCOPE), None)
                .resolve(tenant, &Mention::text("alpha"), 10)
                .await
                .unwrap();
        })
        .await;

    let saved = read(&written.path);
    std::fs::remove_file(&written.path).unwrap();
    assert_eq!(saved["results"], json!({ "numbers": 1 }));
    let saved = saved["plans"].as_array().expect("the record holds plans");
    let classes: Vec<_> = saved.iter().map(|p| p["class"].as_str().unwrap()).collect();
    assert_eq!(classes, ["a Search class", "a Resolution class"]);
    for (entry, function, table) in [
        (&saved[0], "gnomon.search(", "search_token"),
        (&saved[1], "gnomon.resolve(", "node_name"),
    ] {
        assert_eq!(entry["text"], "alpha");
        let statements = entry["statements"].as_array().unwrap();
        top_level_plan(statements, function);
        assert!(reads(statements, table), "{statements:#?}");
    }
}

/// What `scripts/plans.sh` prints for `sql`, piped to it, on `db` in the
/// harness's container, with `options` of its own and `arguments` for psql:
/// psql's rows, then its notices.
fn plans_script(db: &TestDb, options: &[&str], arguments: &[&str], sql: &str) -> (String, String) {
    let mut run = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/plans.sh"
    ))
    .args(options)
    .args([db.name.as_str(), "-At"])
    .args(arguments)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("run scripts/plans.sh");
    run.stdin.take().unwrap().write_all(sql.as_bytes()).unwrap();
    let out = run.wait_with_output().unwrap();
    let text = |bytes| String::from_utf8_lossy(bytes).into_owned();
    let (rows, notices) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{rows}{notices}");
    (rows, notices)
}

#[tokio::test]
async fn the_plans_script_shows_the_nested_plans_of_a_search_as_a_caller() {
    // The script reaches a container the harness started, not a server that
    // `GNOMON_TEST_DATABASE_URL` names.
    if std::env::var_os("GNOMON_TEST_DATABASE_URL").is_some() {
        return;
    }
    let (db, tenant) = one_node().await;
    let tenant = tenant.0.to_string();
    let search = format!(
        "select 'found', count(*), current_user from gnomon.search(
           '{tenant}', 'alpha', null, '2025-01-01', null, '{{}}', 1, 1, 1, 1, 0.5, 0.5, 10);"
    );

    for role in ["gnomon_worker", "authenticated"] {
        let (rows, notices) =
            plans_script(&db, &["-r", role, "-t", &tenant, "-s", SCOPE], &[], &search);
        assert!(rows.contains(&format!("found|1|{role}")), "{rows}{notices}");
        // A statement nested in the Search reads the token table.
        assert!(notices.contains("Query Text:"), "{notices}");
        assert!(notices.contains("on search_token"), "{notices}");
    }
    // A new connection from inside psql, and a psql told to read no startup
    // file, keep the caller role, which cannot bypass row-level security.
    let reconnect = format!(
        "\\connect {}\n{search}\nselect 'bypass', rolbypassrls, rolsuper from pg_roles
         where rolname = current_user;",
        db.name
    );
    let (rows, notices) = plans_script(&db, &["-t", &tenant, "-s", SCOPE], &["-X"], &reconnect);
    assert!(rows.contains("found|1|gnomon_worker"), "{rows}{notices}");
    assert!(rows.contains("bypass|f|f"), "{rows}{notices}");
    assert!(notices.contains("on search_token"), "{notices}");

    // The caller role is the connection's own, so `reset role` keeps it.
    // `set role none` returns to the superuser.
    let roles = "reset role;
        select 'reset', current_user;
        set role none;
        select 'none', rolsuper from pg_roles where rolname = current_user;";
    let (rows, notices) = plans_script(&db, &[], &[], roles);
    assert!(rows.contains("reset|gnomon_worker"), "{rows}{notices}");
    assert!(rows.contains("none|t"), "{rows}{notices}");

    // A quote or a space in a Scope reaches the claims as it is.
    let claims =
        "select current_setting('request.jwt.claims')::json #>> '{app_metadata,scopes,0}';";
    let odd = r#"a "quoted\" Scope"#;
    let (rows, notices) = plans_script(&db, &["-s", odd], &[], claims);
    assert!(rows.lines().any(|l| l == odd), "{rows}{notices}");

    // Under the claims of another Scope, the same Search finds nothing.
    let (rows, notices) = plans_script(&db, &["-t", &tenant, "-s", "elsewhere"], &[], &search);
    assert!(rows.contains("found|0|gnomon_worker"), "{rows}{notices}");
}
