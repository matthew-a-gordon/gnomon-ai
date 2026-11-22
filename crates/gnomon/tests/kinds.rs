//! Seam 1, Kind registration: registering and reading Kinds through the Rust
//! core's Store API against real Postgres (ADR-0002, ADR-0009).
//!
//! The deployment grants Kind registration; these tests grant it to
//! `authenticated`, as a deployment would, except where a test shows what
//! happens without the grant. The adversarial cases live in sql_boundary.rs.

mod support;

use gnomon::{Error, Kind, KindOf, NewKind, Store, TenantId, ROOT_EDGE_KIND, ROOT_NODE_KIND};
use serde_json::{json, Map, Value};
use support::TestDb;
use tokio_postgres::types::Type;

use support::{tenant, user as claims};

/// A migrated database whose deployment lets `authenticated` register Kinds.
async fn registering_db() -> TestDb {
    let db = TestDb::migrated().await;
    db.grant_kind_registration("authenticated").await;
    db
}

async fn store(db: &TestDb) -> Store {
    Store::connect_no_tls(&db.login_url()).await.unwrap()
}

fn node_kind(tenant: TenantId, name: &str, properties: Value) -> NewKind {
    NewKind {
        tenant,
        name: name.to_owned(),
        of: KindOf::Node,
        parent: None,
        properties,
        search: vec![],
    }
}

fn edge_kind(tenant: TenantId, name: &str, source: &[&str], target: &[&str]) -> NewKind {
    NewKind {
        tenant,
        name: name.to_owned(),
        of: KindOf::Edge {
            source: strings(source),
            target: strings(target),
        },
        parent: None,
        properties: json!({ "type": "object" }),
        search: vec![],
    }
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// The message of an `InvalidKind` refusal; panics on anything else.
fn invalid(result: Result<Kind, Error>) -> String {
    match result {
        Err(Error::InvalidKind(m)) => m,
        Err(other) => panic!("expected an invalid Kind, got {other:?}"),
        Ok(k) => panic!("the Kind was registered: {k:?}"),
    }
}

// Root Kinds.

#[tokio::test]
async fn every_ontology_has_a_root_node_kind_and_a_root_edge_kind() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    // Two Tenants that have never registered a Kind.
    for t in [tenant(), tenant()] {
        let caller = store.as_user(claims(t, &["support"]));

        let node = caller
            .read_kind(t, ROOT_NODE_KIND)
            .await
            .unwrap()
            .expect("the root Node Kind exists");
        assert_eq!(node.tenant, t);
        assert_eq!(node.name, "node");
        assert_eq!(node.of, KindOf::Node);
        assert!(node.is_root());
        assert!(node.ancestors.is_empty());
        assert_eq!(node.search, strings(&["name", "aliases"]));
        assert_eq!(node.properties["properties"], json!({}));
        assert!(node.rules.is_empty());

        let edge = caller
            .read_kind(t, ROOT_EDGE_KIND)
            .await
            .unwrap()
            .expect("the root Edge Kind exists");
        assert_eq!(edge.name, "edge");
        assert!(edge.is_root());
        assert_eq!(
            edge.of,
            KindOf::Edge {
                source: strings(&["node"]),
                target: strings(&["node"]),
            },
            "the root Edge Kind joins any Node to any Node"
        );
        assert_eq!(edge.search, strings(&["sentence"]));
    }
}

#[tokio::test]
async fn a_caller_reads_no_kind_of_another_tenant_not_even_a_root_kind() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let (mine, theirs) = (tenant(), tenant());
    let caller = store.as_user(claims(mine, &["support"]));
    assert_eq!(
        caller.read_kind(theirs, ROOT_NODE_KIND).await.unwrap(),
        None
    );
    assert_eq!(caller.read_kind(mine, "no_such_kind").await.unwrap(), None);
}

// The profile and its compiled rules.

/// Evaluates a compiled rule on the server, the way the Store's writes do.
/// `None` would mean jsonpath returned unknown, which a rule must never do.
async fn evaluate(db: &tokio_postgres::Client, rule: &str, value: &Value) -> Option<bool> {
    db.query_typed(
        "select jsonb_path_match($1, $2::jsonpath)",
        &[(value, Type::JSONB), (&rule, Type::TEXT)],
    )
    .await
    .unwrap_or_else(|e| panic!("evaluate {rule} on {value}: {e:?}"))[0]
        .get(0)
}

/// One property schema with values it must accept and values it must refuse.
struct Case {
    schema: Value,
    valid: Vec<Value>,
    invalid: Vec<Value>,
}

fn case(schema: Value, valid: Vec<Value>, invalid: Vec<Value>) -> Case {
    Case {
        schema,
        valid,
        invalid,
    }
}

fn cases() -> Vec<Case> {
    let long = |n: usize| json!("x".repeat(n));
    vec![
        case(
            json!({}),
            vec![json!(1), json!("a"), json!(null), json!([]), json!({})],
            vec![],
        ),
        case(
            json!({ "type": "string" }),
            vec![json!("a"), json!("")],
            vec![json!(1), json!(null), json!(true), json!(["a"]), json!({})],
        ),
        case(
            json!({ "type": "integer" }),
            vec![json!(1), json!(-3), json!(2.0)],
            vec![json!(1.5), json!("1"), json!([1])],
        ),
        case(
            json!({ "type": "number" }),
            vec![json!(1.5), json!(0)],
            vec![json!("1"), json!(true), json!([1])],
        ),
        case(
            json!({ "type": "boolean" }),
            vec![json!(false)],
            vec![json!(0), json!("true")],
        ),
        case(
            json!({ "type": "null" }),
            vec![json!(null)],
            vec![json!(0), json!("")],
        ),
        case(
            json!({ "type": "array" }),
            vec![json!([]), json!([1])],
            vec![json!({}), json!("[]")],
        ),
        case(
            json!({ "type": "object" }),
            vec![json!({}), json!({ "a": 1 })],
            vec![json!([]), json!("x")],
        ),
        case(
            json!({ "type": ["string", "null"] }),
            vec![json!("a"), json!(null)],
            vec![json!(1), json!(["a"])],
        ),
        case(
            json!({ "enum": ["red", "green", 3, null] }),
            vec![json!("red"), json!(3), json!(3.0), json!(null)],
            vec![
                json!("blue"),
                json!(["red"]),
                json!({ "red": 1 }),
                json!(4),
                json!(true),
            ],
        ),
        case(
            json!({ "enum": ["it's \"quoted\"", "back\\slash", "日本"] }),
            vec![
                json!("it's \"quoted\""),
                json!("back\\slash"),
                json!("日本"),
            ],
            vec![json!("its quoted"), json!("backslash")],
        ),
        case(
            json!({ "const": "fixed" }),
            vec![json!("fixed")],
            vec![json!("other"), json!(["fixed"]), json!(null)],
        ),
        case(
            json!({ "type": "number", "minimum": 0, "maximum": 10 }),
            vec![json!(0), json!(10), json!(5.5)],
            vec![json!(-0.1), json!(10.1), json!("5")],
        ),
        case(
            json!({ "exclusiveMinimum": 0, "exclusiveMaximum": 1 }),
            vec![
                json!(0.5),
                json!("not a number, so the bounds do not apply"),
            ],
            vec![json!(0), json!(1)],
        ),
        case(
            json!({ "minimum": -5 }),
            vec![json!(-5), json!("text"), json!([-10])],
            vec![json!(-6)],
        ),
        case(
            json!({ "type": "string", "minLength": 2, "maxLength": 4 }),
            vec![json!("ab"), json!("abcd"), json!("日本"), json!("a\nb")],
            vec![json!("a"), json!("abcde"), json!("")],
        ),
        case(
            json!({ "type": "string", "maxLength": 300 }),
            vec![long(300), json!("")],
            vec![long(301)],
        ),
        case(
            json!({ "type": "string", "minLength": 1000, "maxLength": 1000 }),
            vec![long(1000)],
            vec![long(999), long(1001)],
        ),
        case(
            json!({ "minItems": 1, "maxItems": 2 }),
            vec![json!([1]), json!([1, 2]), json!("not an array")],
            vec![json!([]), json!([1, 2, 3])],
        ),
        case(
            json!({ "type": "array", "items": { "type": "integer" } }),
            vec![json!([]), json!([1, 2])],
            vec![json!([1, "2"]), json!([1.5]), json!([[1]])],
        ),
        case(
            json!({ "type": "array", "items": { "type": "array", "items": { "type": "string" } } }),
            vec![json!([["a"], []])],
            vec![json!([["a", 1]]), json!(["a"])],
        ),
        case(
            json!({ "type": "array", "items": { "enum": ["a", "b"] } }),
            vec![json!(["a", "b", "a"])],
            vec![json!(["a", "c"]), json!([["a"]])],
        ),
        case(
            json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string" },
                    "zip": { "type": "string", "minLength": 5 }
                },
                "required": ["city"]
            }),
            vec![
                json!({ "city": "Leeds" }),
                json!({ "city": "Leeds", "zip": "LS1 4AP" }),
                json!({ "city": "Leeds", "other": 1 }),
            ],
            vec![
                json!({}),
                json!({ "city": 1 }),
                json!({ "city": "Leeds", "zip": "LS1" }),
                json!({ "city": "Leeds", "zip": null }),
                json!([{ "city": "Leeds" }]),
            ],
        ),
        case(
            json!({
                "type": "array",
                "items": { "type": "object", "properties": { "id": { "format": "uuid" } }, "required": ["id"] }
            }),
            vec![json!([{ "id": "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b" }])],
            vec![
                json!([{ "id": "nope" }]),
                json!([{}]),
                json!([{ "id": "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b" }, 1]),
            ],
        ),
        case(
            json!({ "type": "object", "properties": { "we\"ird/key~": { "type": "integer" } } }),
            vec![json!({ "we\"ird/key~": 1 }), json!({})],
            vec![json!({ "we\"ird/key~": "x" })],
        ),
        case(
            json!({ "anyOf": [{ "type": "string", "maxLength": 3 }, { "type": "integer", "minimum": 10 }] }),
            vec![json!("abc"), json!(10)],
            vec![json!("abcd"), json!(9), json!(null), json!(10.5)],
        ),
        case(
            json!({ "format": "date" }),
            vec![json!("2024-02-29"), json!(5)],
            vec![
                json!("2023-02-29"),
                json!("2024-1-5"),
                json!("2024-13-01"),
                json!("20240101"),
                json!("2024-02-29T00:00:00Z"),
            ],
        ),
        case(
            json!({ "type": "string", "format": "date-time" }),
            vec![
                json!("2024-03-14T09:30:00Z"),
                json!("2024-03-14T09:30:00.123456+02:00"),
                json!("2024-03-14T09:30:00-05:00"),
            ],
            vec![
                json!("2024-03-14 09:30:00Z"),
                json!("2024-02-30T09:30:00Z"),
                json!("2024-03-14T25:00:00Z"),
                json!("2024-03-14T09:30:00"),
                json!("2024-03-14"),
            ],
        ),
        case(
            json!({ "type": "string", "format": "uuid" }),
            vec![
                json!("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
                json!("0190A1B2-C3D4-7E5F-8A9B-0C1D2E3F4A5B"),
            ],
            vec![
                json!("not-a-uuid"),
                json!("0190a1b2c3d47e5f8a9b0c1d2e3f4a5b"),
                json!("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5"),
            ],
        ),
    ]
}

#[tokio::test]
async fn every_keyword_of_the_profile_compiles_to_a_rule_that_jsonb_path_match_evaluates() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let cases = cases();
    let properties: Map<String, Value> = cases
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("p{i:02}"), c.schema.clone()))
        .collect();

    let kind = store
        .as_user(claims(t, &["support"]))
        .register_kind(&node_kind(
            t,
            "specimen",
            json!({ "type": "object", "properties": properties }),
        ))
        .await
        .expect("a Kind using every keyword of the profile registers");

    assert_eq!(kind.rules.len(), cases.len(), "one rule per property");
    let server = db.admin().await;
    for (i, c) in cases.iter().enumerate() {
        let name = format!("p{i:02}");
        let rule = &kind.rules[&name];
        for v in &c.valid {
            assert_eq!(
                evaluate(&server, rule, v).await,
                Some(true),
                "{} should accept {v}",
                c.schema
            );
        }
        for v in &c.invalid {
            assert_eq!(
                evaluate(&server, rule, v).await,
                Some(false),
                "{} should refuse {v}",
                c.schema
            );
        }
    }
}

#[tokio::test]
async fn a_registered_node_kind_reads_back_flattened_under_the_root_node_kind() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    let schema = json!({
        "type": "object",
        "properties": {
            "role": { "type": "string" },
            "born": { "type": "string", "format": "date" }
        },
        "required": ["role"]
    });
    let new = NewKind {
        search: strings(&["role"]),
        ..node_kind(t, "person", schema.clone())
    };

    let registered = caller.register_kind(&new).await.unwrap();
    let read = caller
        .read_kind(t, "person")
        .await
        .unwrap()
        .expect("the Kind is visible");

    assert_eq!(registered, read);
    assert_eq!(read.name, "person");
    assert_eq!(read.of, KindOf::Node);
    assert_eq!(read.parent.as_deref(), Some(ROOT_NODE_KIND));
    assert_eq!(read.ancestors, strings(&["node"]));
    assert_eq!(read.properties, schema);
    assert_eq!(read.rules.keys().collect::<Vec<_>>(), ["born", "role"]);
    assert_eq!(read.search, strings(&["name", "aliases", "role"]));
}

#[tokio::test]
async fn keywords_outside_the_profile_are_refused_with_the_keyword_and_where_it_appears() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));

    // A property schema per keyword outside the profile.
    let mut attempts: Vec<(Value, &str, &str)> = [
        "pattern",
        "oneOf",
        "allOf",
        "not",
        "$ref",
        "$defs",
        "if",
        "additionalProperties",
        "patternProperties",
        "prefixItems",
        "uniqueItems",
        "contains",
        "multipleOf",
        "dependentRequired",
        "description",
        "title",
        "default",
        "examples",
    ]
    .into_iter()
    .map(|kw| (json!({ "code": { kw: "x" } }), kw, "/properties/code"))
    .collect();
    // The same refusal wherever the keyword is nested.
    attempts.extend([
        (
            json!({ "code": { "type": "string", "pattern": "^[A-Z]{3}$" } }),
            "pattern",
            "/properties/code",
        ),
        (
            json!({ "tags": { "type": "array", "items": { "type": "string", "pattern": "x" } } }),
            "pattern",
            "/properties/tags/items",
        ),
        (
            json!({ "v": { "anyOf": [{ "type": "string" }, { "oneOf": [] }] } }),
            "oneOf",
            "/properties/v/anyOf/1",
        ),
        (
            json!({ "address": { "type": "object", "properties": { "zip/code": { "pattern": "x" } } } }),
            "pattern",
            "/properties/address/properties/zip~1code",
        ),
    ]);

    for (properties, keyword, at) in attempts {
        let m = invalid(
            caller
                .register_kind(&node_kind(
                    t,
                    "account",
                    json!({ "type": "object", "properties": properties }),
                ))
                .await,
        );
        assert!(
            m.contains(&format!(
                "unsupported JSON Schema keyword \"{keyword}\" at {at}"
            )),
            "the refusal of {keyword} at {at} says: {m}"
        );
    }

    let m = invalid(
        caller
            .register_kind(&node_kind(
                t,
                "account",
                json!({ "type": "object", "additionalProperties": false }),
            ))
            .await,
    );
    assert!(
        m.contains("\"additionalProperties\" at the top level"),
        "{m}"
    );

    assert_eq!(
        caller.read_kind(t, "account").await.unwrap(),
        None,
        "no refused Kind was written"
    );
}

#[tokio::test]
async fn malformed_schemas_are_refused_with_a_clear_reason() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    let prop = |schema: Value| json!({ "type": "object", "properties": { "p": schema } });

    let attempts = [
        (
            prop(json!({ "format": "email" })),
            "unsupported format \"email\"",
        ),
        (prop(json!({ "type": "date" })), "unknown type \"date\""),
        (prop(json!({ "type": [] })), "\"type\" at /properties/p"),
        (
            prop(json!({ "items": [{ "type": "string" }] })),
            "tuple items are not in the profile",
        ),
        (
            prop(json!({ "enum": [{ "a": 1 }] })),
            "only strings, numbers, booleans or null",
        ),
        (
            prop(json!({ "const": ["a"] })),
            "only strings, numbers, booleans or null",
        ),
        (prop(json!({ "enum": [] })), "non-empty list"),
        (
            prop(json!({ "minLength": -1 })),
            "whole number of at least 0",
        ),
        (
            prop(json!({ "maxItems": 1.5 })),
            "whole number of at least 0",
        ),
        (
            prop(json!({ "maximum": "10" })),
            "\"maximum\" at /properties/p must be a number",
        ),
        (
            prop(json!({ "maxLength": 1001 })),
            "string length bounds up to 1000",
        ),
        (prop(json!({ "anyOf": [] })), "non-empty list of schemas"),
        (
            prop(json!({ "required": ["a", "a"] })),
            "distinct property names",
        ),
        (
            prop(json!({ "type": ["string", "string"] })),
            "non-empty list of distinct ones",
        ),
        (
            json!({ "properties": { "a": {} }, "required": ["a", "a"] }),
            "\"required\" at the top level must be a list of distinct property names",
        ),
        (
            prop(json!(true)),
            "the schema at /properties/p must be a JSON object",
        ),
        (json!({ "type": "string" }), "must have type \"object\""),
        (
            json!({ "type": "object", "minProperties": 1 }),
            "\"minProperties\" at the top level",
        ),
        (
            json!({ "type": "object", "enum": [1] }),
            "cannot apply to a whole Kind",
        ),
        (json!({ "properties": [] }), "must be an object of schemas"),
        (
            json!({ "properties": {}, "required": ["missing"] }),
            "required property \"missing\"",
        ),
        (json!(null), "must be a JSON Schema object"),
    ];
    for (properties, expected) in attempts {
        let m = invalid(
            caller
                .register_kind(&node_kind(t, "account", properties.clone()))
                .await,
        );
        assert!(
            m.contains(expected),
            "{properties}: expected {expected:?} in {m:?}"
        );
    }
}

#[tokio::test]
async fn kind_names_must_be_identifiers_and_the_root_names_are_taken() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    for name in ["", "9lives", "has space", "dash-ed", &"x".repeat(64)] {
        let m = invalid(caller.register_kind(&node_kind(t, name, json!({}))).await);
        assert!(m.contains("must be a letter followed by"), "{name:?}: {m}");
    }
    for name in [ROOT_NODE_KIND, ROOT_EDGE_KIND] {
        let m = invalid(caller.register_kind(&node_kind(t, name, json!({}))).await);
        assert!(m.contains("is a root Kind"), "{name}: {m}");
    }
    caller
        .register_kind(&node_kind(t, &format!("A{}", "x".repeat(62)), json!({})))
        .await
        .expect("a 63-character name registers");
}

// Specialization.

#[tokio::test]
async fn a_child_kind_inherits_its_parents_properties_required_properties_and_search_marks() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));

    let organisation = caller
        .register_kind(&NewKind {
            search: strings(&["legal_name"]),
            ..node_kind(
                t,
                "organisation",
                json!({
                    "type": "object",
                    "properties": {
                        "legal_name": { "type": "string", "minLength": 1 },
                        "founded": { "type": "string", "format": "date" }
                    },
                    "required": ["legal_name"]
                }),
            )
        })
        .await
        .unwrap();
    let customer = caller
        .register_kind(&NewKind {
            parent: Some("organisation".into()),
            search: strings(&["account_ref"]),
            ..node_kind(
                t,
                "customer",
                json!({
                    "type": "object",
                    "properties": { "account_ref": { "type": "string" } },
                    "required": ["account_ref", "founded"]
                }),
            )
        })
        .await
        .unwrap();
    // A grandchild that declares nothing of its own.
    let key_customer = caller
        .register_kind(&NewKind {
            parent: Some("customer".into()),
            ..node_kind(t, "key_customer", json!({}))
        })
        .await
        .unwrap();

    assert_eq!(customer.parent.as_deref(), Some("organisation"));
    assert_eq!(customer.ancestors, strings(&["node", "organisation"]));
    assert_eq!(
        customer.properties,
        json!({
            "type": "object",
            "properties": {
                "legal_name": { "type": "string", "minLength": 1 },
                "founded": { "type": "string", "format": "date" },
                "account_ref": { "type": "string" }
            },
            "required": ["legal_name", "account_ref", "founded"]
        })
    );
    assert_eq!(
        customer.search,
        strings(&["name", "aliases", "legal_name", "account_ref"])
    );
    // The inherited rules are the parent's rules.
    for p in ["legal_name", "founded"] {
        assert_eq!(customer.rules[p], organisation.rules[p], "{p}");
    }
    assert!(customer.rules.contains_key("account_ref"));

    assert_eq!(
        key_customer.ancestors,
        strings(&["node", "organisation", "customer"])
    );
    assert_eq!(key_customer.properties, customer.properties);
    assert_eq!(key_customer.rules, customer.rules);
    assert_eq!(key_customer.search, customer.search);
}

#[tokio::test]
async fn a_child_kind_cannot_redeclare_an_inherited_property_or_specialize_another_kind_of() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    caller
        .register_kind(&node_kind(
            t,
            "organisation",
            json!({ "properties": { "legal_name": { "type": "string" } } }),
        ))
        .await
        .unwrap();
    caller
        .register_kind(&edge_kind(t, "works_at", &[], &[]))
        .await
        .unwrap();

    let m = invalid(
        caller
            .register_kind(&NewKind {
                parent: Some("organisation".into()),
                ..node_kind(
                    t,
                    "customer",
                    json!({ "properties": { "legal_name": { "type": "integer" } } }),
                )
            })
            .await,
    );
    assert!(
        m.contains("\"legal_name\" is inherited from \"organisation\""),
        "{m}"
    );

    let m = invalid(
        caller
            .register_kind(&NewKind {
                parent: Some("works_at".into()),
                ..node_kind(t, "customer", json!({}))
            })
            .await,
    );
    assert!(m.contains("cannot specialize \"works_at\""), "{m}");

    let m = invalid(
        caller
            .register_kind(&NewKind {
                parent: Some(ROOT_EDGE_KIND.into()),
                ..node_kind(t, "customer", json!({}))
            })
            .await,
    );
    assert!(m.contains("cannot specialize \"edge\""), "{m}");

    let m = invalid(
        caller
            .register_kind(&NewKind {
                parent: Some("company".into()),
                ..node_kind(t, "customer", json!({}))
            })
            .await,
    );
    assert!(
        m.contains("parent Kind \"company\" is not registered"),
        "{m}"
    );
}

#[tokio::test]
async fn search_marks_name_properties_and_the_built_in_fields_are_reserved() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));

    let m = invalid(
        caller
            .register_kind(&NewKind {
                search: strings(&["nickname"]),
                ..node_kind(
                    t,
                    "person",
                    json!({ "properties": { "role": { "type": "string" } } }),
                )
            })
            .await,
    );
    assert!(
        m.contains("search mark \"nickname\" names no property"),
        "{m}"
    );

    let m = invalid(
        caller
            .register_kind(&NewKind {
                search: strings(&["sentence"]),
                ..node_kind(t, "person", json!({}))
            })
            .await,
    );
    assert!(
        m.contains("search mark \"sentence\""),
        "a Node Kind has no Sentence: {m}"
    );

    for (kind, reserved) in [
        (
            node_kind(t, "person", json!({ "properties": { "name": {} } })),
            "name",
        ),
        (
            node_kind(t, "person", json!({ "properties": { "aliases": {} } })),
            "aliases",
        ),
        (
            NewKind {
                properties: json!({ "properties": { "sentence": {} } }),
                ..edge_kind(t, "knows", &[], &[])
            },
            "sentence",
        ),
    ] {
        let m = invalid(caller.register_kind(&kind).await);
        assert!(
            m.contains(&format!("property name \"{reserved}\" is reserved")),
            "{m}"
        );
    }

    // Marking an inherited field again changes nothing.
    let person = caller
        .register_kind(&NewKind {
            search: strings(&["aliases", "role"]),
            ..node_kind(
                t,
                "person",
                json!({ "properties": { "role": { "type": "string" } } }),
            )
        })
        .await
        .unwrap();
    assert_eq!(person.search, strings(&["name", "aliases", "role"]));
}

// Edge Kinds and the Node Kinds they join.

#[tokio::test]
async fn an_edge_kind_stores_the_node_kinds_it_may_join_and_a_child_narrows_or_inherits_them() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    for kind in [
        node_kind(t, "person", json!({})),
        node_kind(t, "organisation", json!({})),
        NewKind {
            parent: Some("organisation".into()),
            ..node_kind(t, "customer", json!({}))
        },
    ] {
        caller.register_kind(&kind).await.unwrap();
    }

    let works_at = caller
        .register_kind(&NewKind {
            properties: json!({ "properties": { "since": { "type": "string", "format": "date" } } }),
            ..edge_kind(t, "works_at", &["person"], &["organisation"])
        })
        .await
        .unwrap();
    assert_eq!(
        works_at.of,
        KindOf::Edge {
            source: strings(&["person"]),
            target: strings(&["organisation"]),
        }
    );
    assert_eq!(works_at.parent.as_deref(), Some(ROOT_EDGE_KIND));
    assert_eq!(
        works_at.search,
        strings(&["sentence"]),
        "Edge Kinds search the Sentence"
    );
    assert_eq!(
        caller.read_kind(t, "works_at").await.unwrap().unwrap().of,
        works_at.of,
        "the endpoints are stored with the Kind"
    );

    // Declaring nothing inherits the parent's endpoints.
    let employed_by = caller
        .register_kind(&NewKind {
            parent: Some("works_at".into()),
            ..edge_kind(t, "employed_by", &[], &[])
        })
        .await
        .unwrap();
    assert_eq!(employed_by.of, works_at.of);
    assert!(employed_by.rules.contains_key("since"));

    // A child may narrow an end to a Kind below the parent's.
    let manages_account = caller
        .register_kind(&NewKind {
            parent: Some("works_at".into()),
            ..edge_kind(t, "manages_account", &[], &["customer"])
        })
        .await
        .unwrap();
    assert_eq!(
        manages_account.of,
        KindOf::Edge {
            source: strings(&["person"]),
            target: strings(&["customer"]),
        }
    );

    // An Edge Kind under the root may join any Node Kinds, the root included.
    let mentions = caller
        .register_kind(&edge_kind(
            t,
            "mentions",
            &["node"],
            &["person", "organisation"],
        ))
        .await
        .unwrap();
    assert_eq!(
        mentions.of,
        KindOf::Edge {
            source: strings(&["node"]),
            target: strings(&["organisation", "person"]),
        }
    );
    let related = caller
        .register_kind(&edge_kind(t, "related_to", &[], &[]))
        .await
        .unwrap();
    assert_eq!(
        related.of,
        KindOf::Edge {
            source: strings(&["node"]),
            target: strings(&["node"]),
        }
    );
}

#[tokio::test]
async fn an_edge_kind_may_join_only_registered_node_kinds_within_what_its_parent_allows() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    for kind in [
        node_kind(t, "person", json!({})),
        node_kind(t, "organisation", json!({})),
        edge_kind(t, "knows", &["person"], &["person"]),
    ] {
        caller.register_kind(&kind).await.unwrap();
    }

    let attempts = [
        (
            edge_kind(t, "orbits", &["planet"], &["node"]),
            "source Kind \"planet\" is not a registered Node Kind",
        ),
        (
            edge_kind(t, "cites", &["node"], &["knows"]),
            "target Kind \"knows\" is not a registered Node Kind",
        ),
        (
            edge_kind(t, "cites", &["node"], &["edge"]),
            "target Kind \"edge\" is not a registered Node Kind",
        ),
        (
            NewKind {
                parent: Some("knows".into()),
                ..edge_kind(t, "employs", &["organisation"], &[])
            },
            "source Kind \"organisation\" is outside what the parent Edge Kind allows (person)",
        ),
        (
            NewKind {
                parent: Some("knows".into()),
                ..edge_kind(t, "employs", &[], &["node"])
            },
            "target Kind \"node\" is outside what the parent Edge Kind allows",
        ),
        (
            NewKind {
                parent: Some("person".into()),
                ..edge_kind(t, "employs", &[], &[])
            },
            "cannot specialize \"person\", which is a Kind of node",
        ),
    ];
    for (kind, expected) in attempts {
        let m = invalid(caller.register_kind(&kind).await);
        assert!(
            m.contains(expected),
            "{}: expected {expected:?} in {m:?}",
            kind.name
        );
    }
}

// Registering again, Tenants and Scopes, and the grant.

#[tokio::test]
async fn registering_the_same_kind_again_changes_nothing_and_another_definition_conflicts() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    let person = NewKind {
        search: strings(&["role"]),
        ..node_kind(
            t,
            "person",
            json!({ "properties": { "role": { "type": "string" } } }),
        )
    };

    let first = caller.register_kind(&person).await.unwrap();
    let again = caller
        .register_kind(&person)
        .await
        .expect("the same Kind registers again");
    assert_eq!(again, first);

    let changed = NewKind {
        properties: json!({ "properties": { "role": { "type": "integer" } } }),
        ..person.clone()
    };
    match caller.register_kind(&changed).await {
        Err(Error::KindConflict(m)) => assert!(
            m.contains("\"person\" is already registered with a different definition"),
            "{m}"
        ),
        other => panic!("expected a Kind conflict, got {other:?}"),
    }
    assert_eq!(caller.read_kind(t, "person").await.unwrap(), Some(first));
}

#[tokio::test]
async fn each_tenant_has_its_own_ontology_which_every_scope_of_the_tenant_shares() {
    let db = registering_db().await;
    let store = store(&db).await;
    let (a, b) = (tenant(), tenant());
    let in_support = store.as_user(claims(a, &["support"]));
    in_support
        .register_kind(&node_kind(
            a,
            "person",
            json!({ "properties": { "role": { "type": "string" } } }),
        ))
        .await
        .unwrap();

    let in_billing = store.as_user(claims(a, &["billing"]));
    assert!(
        in_billing.read_kind(a, "person").await.unwrap().is_some(),
        "another Scope of the Tenant reads the Kind"
    );
    let worker = store.as_worker(a, gnomon::Scope::new("billing"));
    assert!(
        worker.read_kind(a, "person").await.unwrap().is_some(),
        "a worker reads it"
    );

    let other = store.as_user(claims(b, &["support"]));
    assert_eq!(other.read_kind(a, "person").await.unwrap(), None);
    assert_eq!(other.read_kind(b, "person").await.unwrap(), None);
    let theirs = other
        .register_kind(&node_kind(
            b,
            "person",
            json!({ "properties": { "age": { "type": "integer" } } }),
        ))
        .await
        .expect("another Tenant registers the same name with its own definition");
    assert_eq!(theirs.rules.keys().collect::<Vec<_>>(), ["age"]);
}

#[tokio::test]
async fn without_the_deployments_grant_registration_fails_with_insufficient_privilege() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    match store
        .as_user(claims(t, &["support"]))
        .register_kind(&node_kind(t, "person", json!({})))
        .await
    {
        Err(e) => assert_eq!(
            e.sqlstate(),
            Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{e:?}"
        ),
        Ok(k) => panic!("registered without the grant: {k:?}"),
    }
}

#[tokio::test]
async fn a_caller_with_the_grant_registers_only_in_its_own_tenant() {
    let db = registering_db().await;
    let store = store(&db).await;
    let (mine, theirs) = (tenant(), tenant());
    match store
        .as_user(claims(mine, &["support"]))
        .register_kind(&node_kind(theirs, "person", json!({})))
        .await
    {
        Err(e) => assert_eq!(
            e.sqlstate(),
            Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{e:?}"
        ),
        Ok(k) => panic!("registered into another Tenant: {k:?}"),
    }
}

#[tokio::test]
async fn a_schema_that_would_compile_to_an_unbounded_rule_is_refused() {
    let db = registering_db().await;
    let store = store(&db).await;
    let t = tenant();
    let caller = store.as_user(claims(t, &["support"]));
    let nest = |depth: usize, wrap: &dyn Fn(Value) -> Value| {
        (0..depth).fold(json!({ "type": "string" }), |inner, _| wrap(inner))
    };
    let kind = |schema: Value| node_kind(t, "deep", json!({ "properties": { "p": schema } }));

    // Each array in an array doubles the rule.
    let arrays = nest(20, &|inner| json!({ "type": "array", "items": inner }));
    let m = invalid(caller.register_kind(&kind(arrays)).await);
    assert!(
        m.contains("compiles to a rule longer than 65536 characters"),
        "{m}"
    );

    let objects = nest(
        40,
        &|inner| json!({ "type": "object", "properties": { "x": inner } }),
    );
    let m = invalid(caller.register_kind(&kind(objects)).await);
    assert!(
        m.contains("nests deeper than the profile allows (32 levels)"),
        "{m}"
    );

    // Ordinary nesting still registers.
    let shallow = nest(4, &|inner| json!({ "type": "array", "items": inner }));
    caller
        .register_kind(&kind(shallow))
        .await
        .expect("four nested arrays register");
}
