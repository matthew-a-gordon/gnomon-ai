//! The conversion between a [`Timestamp`] and Postgres's `timestamptz`. No
//! released `postgres-types` converts `jiff` 1.0, so gnomon has its own until
//! one does (ADR-0001, #109).
//!
//! The database tests compile this file too, to run their own SQL.

use std::error::Error;

use bytes::BytesMut;
use jiff::Timestamp;
use postgres_protocol::types::{timestamp_from_sql, timestamp_to_sql};
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};

/// The microsecond count at 2000-01-01 UTC, where `timestamptz` counts from.
const POSTGRES_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// A [`Timestamp`] as a statement parameter or a column.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timestamptz(pub(crate) Timestamp);

impl<'a> FromSql<'a> for Timestamptz {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        // `infinity` is the largest count, so it overflows the addition.
        // `-infinity`, the smallest, is out of a `Timestamp`'s range.
        timestamp_from_sql(raw)?
            .checked_add(POSTGRES_EPOCH_MICROS)
            .and_then(|micros| Timestamp::from_microsecond(micros).ok())
            .map(Timestamptz)
            .ok_or_else(|| "value too large to decode".into())
    }

    accepts!(TIMESTAMPTZ);
}

impl ToSql for Timestamptz {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        let since_2000 = self.0.as_nanosecond() - i128::from(POSTGRES_EPOCH_MICROS) * 1_000;
        // The division truncates toward zero, which is toward 2000-01-01.
        timestamp_to_sql(i64::try_from(since_2000 / 1_000)?, out);
        Ok(IsNull::No)
    }

    accepts!(TIMESTAMPTZ);
    to_sql_checked!();
}
