//! Installing gnomon's migration files into a second database on one
//! cluster, where another role created the gnomon roles (ticket #80).
//!
//! The roles are cluster-wide, so the files find `authenticated`,
//! `gnomon_worker`, `gnomon_definer`, `gnomon_writer` and `gnomon_eraser`
//! already there and create none. The last file hands the definer functions
//! to `gnomon_definer`, `gnomon_writer` and `gnomon_eraser`, which needs the
//! migrating role to SET ROLE to each for that step. A role that created them
//! holds ADMIN on each and grants itself SET for the step. A second migrating
//! role holds nothing, so the last file fails until a role with ADMIN on all
//! three grants it ADMIN on all three too, with neither SET nor INHERIT. The
//! README states the requirement.

mod support;

use support::{Server, TestDb, quote_ident};
use tokio_postgres::{Client, NoTls};

const PASSWORD: &str = "gnomon-second-migrator";

async fn connect_as(db: &TestDb, role: &str) -> Client {
    let addr = db.direct_server_addr().await;
    let mut config = tokio_postgres::Config::new();
    config
        .host(addr.ip().to_string())
        .port(addr.port())
        .dbname(&db.name)
        .user(role)
        .password(PASSWORD);
    let (client, conn) = config.connect(NoTls).await.expect("connect");
    tokio::spawn(conn);
    client
}

/// Applies one file in one transaction.
async fn apply(client: &mut Client, sql: &str) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.batch_execute(sql).await?;
    tx.commit().await
}

#[tokio::test]
async fn a_second_migrating_role_needs_admin_on_the_definer_roles_and_nothing_more() {
    // `roles` runs the cluster-wide statements. `second` is the database a
    // second team applies the files to, as the role `migrator`, which owns it
    // and may not create roles.
    let roles = TestDb::empty_with_login().await;
    let second = TestDb::empty_with_login().await;
    let migrator = format!("gnomon_test_migrator_{}", uuid::Uuid::new_v4().simple());
    let q = quote_ident(&migrator);
    let admin = roles.admin().await;
    admin
        .batch_execute(&format!(
            "create role {q} login password '{PASSWORD}';
             alter database {} owner to {q};",
            quote_ident(&second.name)
        ))
        .await
        .unwrap();
    // The host's administrator installs the extensions, as a managed host
    // lets a project's own role do.
    second
        .admin()
        .await
        .batch_execute(
            "create extension if not exists vector; create extension if not exists btree_gist",
        )
        .await
        .unwrap();

    let files = support::migration_files(support::DEFAULT_DIMENSION);
    let (last, earlier) = files.split_last().unwrap();
    let mut client = connect_as(&second, &migrator).await;
    for (file, sql) in earlier {
        apply(&mut client, sql)
            .await
            .unwrap_or_else(|e| panic!("{file}: {}", support::chain(&e)));
    }
    let err = apply(&mut client, &last.1)
        .await
        .expect_err("the last file handed the definer functions over without the grant");
    assert_eq!(
        err.code(),
        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{}",
        support::chain(&err)
    );
    assert!(
        support::chain(&err).contains("gnomon_definer"),
        "{}",
        support::chain(&err)
    );

    // ADMIN on one of the three roles is not enough, nor on two.
    admin
        .batch_execute(&format!(
            "grant gnomon_definer to {q} with admin option, inherit false, set false"
        ))
        .await
        .unwrap();
    let err = {
        let _lock = Server::get().await.lock();
        apply(&mut client, &last.1)
            .await
            .expect_err("the last file handed the write functions over without the grant")
    };
    assert!(
        support::chain(&err).contains("gnomon_writer"),
        "{}",
        support::chain(&err)
    );
    admin
        .batch_execute(&format!(
            "grant gnomon_writer to {q} with admin option, inherit false, set false"
        ))
        .await
        .unwrap();
    let err = {
        let _lock = Server::get().await.lock();
        apply(&mut client, &last.1)
            .await
            .expect_err("the last file handed Erasure over without the grant")
    };
    assert!(
        support::chain(&err).contains("gnomon_eraser"),
        "{}",
        support::chain(&err)
    );
    admin
        .batch_execute(&format!(
            "grant gnomon_eraser to {q} with admin option, inherit false, set false"
        ))
        .await
        .unwrap();
    {
        // The last file lets the migrator act as `gnomon_definer`,
        // `gnomon_writer` and `gnomon_eraser` for one step, which the test of
        // the definer roles' members must not see.
        let _lock = Server::get().await.lock();
        apply(&mut client, &last.1)
            .await
            .unwrap_or_else(|e| panic!("{}: {}", last.0, support::chain(&e)));
    }
    let owner: String = client
        .query_one(
            &format!(
                "select proowner::regrole::text from pg_proc where oid = '{}'::regprocedure",
                support::REGISTER_KIND
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(owner, "gnomon_definer");
    let acting: bool = client
        .query_one(
            "select pg_has_role(current_user, 'gnomon_definer', 'SET')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!acting, "the migrator kept SET on gnomon_definer");
    let (owner, acting): (String, bool) = {
        let row = client
            .query_one(
                "select proowner::regrole::text, pg_has_role(current_user, 'gnomon_writer', 'SET')
                 from pg_proc where oid = 'gnomon.record_episode(uuid, text, text, timestamptz, integer)'::regprocedure",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!(owner, "gnomon_writer");
    assert!(!acting, "the migrator kept SET on gnomon_writer");
    let (owner, acting): (String, bool) = {
        let row = client
            .query_one(
                "select proowner::regrole::text, pg_has_role(current_user, 'gnomon_eraser', 'SET')
                 from pg_proc where oid = 'gnomon.erase_episode(uuid, uuid)'::regprocedure",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!(owner, "gnomon_eraser");
    assert!(!acting, "the migrator kept SET on gnomon_eraser");
    support::connect_and_check(&second.login_url(), &support::connect_options())
        .await
        .expect("the Store accepts the second database");

    drop(client);
    drop(second);
    admin
        .batch_execute(&format!("drop role {q}"))
        .await
        .unwrap();
}
