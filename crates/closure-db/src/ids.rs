use twilight_model::id::Id;

use crate::error::DbError;

/// Discord snowflakes are 64-bit unsigned but always below 2^63, so they fit `BIGINT`. The
/// conversion is checked in one place instead of using `as` casts around the code base.
pub(crate) fn to_db<M>(id: Id<M>) -> Result<i64, DbError> {
    i64::try_from(id.get()).map_err(|_| DbError::IdOutOfRange(id.get()))
}

pub(crate) fn from_db<M>(value: i64) -> Result<Id<M>, DbError> {
    u64::try_from(value)
        .ok()
        .and_then(Id::new_checked)
        .ok_or(DbError::InvalidStoredId(value))
}

#[cfg(test)]
mod tests {
    use closure_core::event::GuildId;

    use super::*;

    #[test]
    fn ids_round_trip() {
        let id: GuildId = Id::new(1_563_460_727_941_890_048);
        assert_eq!(
            from_db::<twilight_model::id::marker::GuildMarker>(to_db(id).unwrap()).unwrap(),
            id
        );
    }

    #[test]
    fn out_of_range_ids_are_errors_not_wraparounds() {
        let too_big: GuildId = Id::new(u64::MAX);
        assert!(matches!(to_db(too_big), Err(DbError::IdOutOfRange(_))));
        assert!(matches!(
            from_db::<twilight_model::id::marker::GuildMarker>(-1),
            Err(DbError::InvalidStoredId(-1))
        ));
        assert!(matches!(
            from_db::<twilight_model::id::marker::GuildMarker>(0),
            Err(DbError::InvalidStoredId(0))
        ));
    }
}
