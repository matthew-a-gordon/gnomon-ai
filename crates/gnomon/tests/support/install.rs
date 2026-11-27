//! What the consumer-path tests share: a database prepared as a host
//! prepares one, and the checks that the files installed what they should.

use super::{TestDb, quote_ident};

/// Where pgvector goes in a test of a consumer's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extensions {
    /// The database has no `extensions` schema, so the first file creates
    /// pgvector and btree_gist in the migrating role's default schema.
    Default,
    /// The database has an `extensions` schema, so the first file creates
    /// both there. The migration role owns it, so it may grant USAGE on it,
    /// as `postgres` may on Supabase's.
    InExtensions,
}

/// Prepares `db` for `placement` and returns the schema the first file
/// should create pgvector and btree_gist in. Neither extension is installed
/// yet, so the first file creates both.
pub async fn prepare(db: &TestDb, placement: Extensions) -> String {
    let admin = db.admin().await;
    let installed: i64 = admin
        .query_one(
            "select count(*) from pg_extension where extname in ('vector', 'btree_gist')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(installed, 0, "the new database already has an extension");
    let has_extensions: bool = admin
        .query_one(
            "select exists (select from pg_namespace where nspname = 'extensions')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    match placement {
        Extensions::InExtensions => {
            if !has_extensions {
                let owner: String = db
                    .operator()
                    .await
                    .query_one("select current_user::text", &[])
                    .await
                    .unwrap()
                    .get(0);
                admin
                    .batch_execute(&format!(
                        "create schema extensions authorization {}",
                        quote_ident(&owner)
                    ))
                    .await
                    .unwrap();
            }
            "extensions".into()
        }
        Extensions::Default => {
            assert!(
                !has_extensions,
                "the image's new databases have an `extensions` schema"
            );
            "public".into()
        }
    }
}

/// Checks that the files created pgvector and btree_gist in `schema`, and
/// that the Store accepts the schema they left.
pub async fn assert_installed(db: &TestDb, schema: &str) {
    let rows = db
        .admin()
        .await
        .query(
            "select e.extname::text, n.nspname::text from pg_extension e
             join pg_namespace n on n.oid = e.extnamespace
             where e.extname in ('vector', 'btree_gist') order by 1",
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        found,
        [
            ("btree_gist".to_owned(), schema.to_owned()),
            ("vector".to_owned(), schema.to_owned())
        ],
        "the extensions the first file created"
    );
    super::connect_and_check(&db.login_url(), &super::connect_options())
        .await
        .expect("the Store accepts the schema");
}
