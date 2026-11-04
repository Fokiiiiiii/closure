use closure_core::event::GuildId;
use serde_json::Value;

use crate::{Db, DbError, ids::to_db};

impl Db {
    pub async fn record_member_activity(
        &self,
        guild: GuildId,
        joined: bool,
    ) -> Result<(), DbError> {
        sqlx::query("INSERT INTO guild_member_activity (guild_id, hour, joins, leaves) VALUES ($1, date_trunc('hour', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC', $2, $3) ON CONFLICT (guild_id, hour) DO UPDATE SET joins = guild_member_activity.joins + EXCLUDED.joins, leaves = guild_member_activity.leaves + EXCLUDED.leaves")
            .bind(to_db(guild)?)
            .bind(i32::from(joined))
            .bind(i32::from(!joined))
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn purge_member_activity(&self) -> Result<u64, DbError> {
        Ok(sqlx::query(
            "DELETE FROM guild_member_activity WHERE hour < now() - interval '400 days'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn guild_stats(
        &self,
        guild: GuildId,
        days: i32,
        offset: i64,
    ) -> Result<Value, DbError> {
        let value = sqlx::query_scalar::<_, Value>(r"
WITH params AS (
  SELECT now() AS now, make_interval(secs => $3::double precision) AS off
), bounds AS (
  SELECT now, off, ((now AT TIME ZONE 'UTC' + off)::date - ($2 - 1)) AS first_day,
         (now AT TIME ZONE 'UTC' + off)::date AS last_day FROM params
), dates AS (
  SELECT (first_day + n)::date AS day FROM bounds, generate_series(0, $2 - 1) n
), member_since AS (
  SELECT min(hour) AS since FROM guild_member_activity WHERE guild_id = $1
), message_since AS (
  SELECT min(created_at) AS since FROM saved_messages WHERE guild_id = $1
), member_daily AS (
  SELECT (hour AT TIME ZONE 'UTC' + off)::date AS day, sum(joins)::bigint AS joins, sum(leaves)::bigint AS leaves
  FROM guild_member_activity, bounds WHERE guild_id = $1
    AND hour >= ((first_day::timestamp - off) AT TIME ZONE 'UTC') AND hour <= now GROUP BY 1
), message_daily AS (
  SELECT (created_at AT TIME ZONE 'UTC' + off)::date AS day, count(*) AS count
  FROM saved_messages, bounds WHERE guild_id = $1 AND NOT author_bot
    AND created_at >= ((first_day::timestamp - off) AT TIME ZONE 'UTC') AND created_at <= now GROUP BY 1
), case_daily AS (
  SELECT (created_at AT TIME ZONE 'UTC' + off)::date AS day, kind, count(*) AS count
  FROM cases, bounds WHERE guild_id = $1
    AND created_at >= ((first_day::timestamp - off) AT TIME ZONE 'UTC') AND created_at <= now GROUP BY 1, 2
), case_days AS (
  SELECT day, jsonb_object_agg(kind, count) AS kinds FROM case_daily GROUP BY day
), case_kinds AS (
  SELECT kind, sum(count)::bigint AS count FROM case_daily GROUP BY kind
)
SELECT jsonb_build_object(
  'generated_at_unix', floor(extract(epoch FROM (SELECT now FROM bounds)))::bigint,
  'members', jsonb_build_object(
    'since_unix', (SELECT extract(epoch FROM since)::bigint FROM member_since),
    'joins', (SELECT coalesce(sum(joins), 0) FROM member_daily),
    'leaves', (SELECT coalesce(sum(leaves), 0) FROM member_daily),
    'by_day', (SELECT jsonb_agg(jsonb_build_object('date', d.day::text,
      'joins', CASE WHEN s.since IS NULL OR ((d.day + 1)::timestamp - b.off) AT TIME ZONE 'UTC' <= s.since THEN NULL ELSE coalesce(m.joins, 0) END,
      'leaves', CASE WHEN s.since IS NULL OR ((d.day + 1)::timestamp - b.off) AT TIME ZONE 'UTC' <= s.since THEN NULL ELSE coalesce(m.leaves, 0) END) ORDER BY d.day)
      FROM dates d CROSS JOIN member_since s CROSS JOIN bounds b LEFT JOIN member_daily m ON m.day = d.day)),
  'messages', jsonb_build_object(
    'since_unix', (SELECT floor(extract(epoch FROM since))::bigint FROM message_since),
    'total', (SELECT coalesce(sum(count), 0) FROM message_daily),
    'by_day', (SELECT jsonb_agg(jsonb_build_object('date', d.day::text,
      'count', CASE WHEN s.since IS NULL OR ((d.day + 1)::timestamp - b.off) AT TIME ZONE 'UTC' <= s.since THEN NULL ELSE coalesce(m.count, 0) END) ORDER BY d.day)
      FROM dates d CROSS JOIN message_since s CROSS JOIN bounds b LEFT JOIN message_daily m ON m.day = d.day)),
  'cases', jsonb_build_object(
    'total', (SELECT coalesce(sum(count), 0) FROM case_daily),
    'by_kind', (SELECT coalesce(jsonb_object_agg(kind, count), '{}'::jsonb) FROM case_kinds),
    'by_day', (SELECT jsonb_agg(jsonb_build_object('date', d.day::text, 'kinds', coalesce(c.kinds, '{}'::jsonb)) ORDER BY d.day) FROM dates d LEFT JOIN case_days c ON c.day = d.day),
    'recent', (SELECT coalesce(jsonb_agg(jsonb_build_object('number', number, 'kind', kind, 'module', module, 'target', target::text, 'moderator', moderator::text, 'reason', reason, 'created_at_unix', floor(extract(epoch FROM created_at))::bigint, 'revoked', revoked_at IS NOT NULL) ORDER BY created_at DESC, number DESC), '[]'::jsonb) FROM (SELECT * FROM cases WHERE guild_id = $1 ORDER BY created_at DESC, number DESC LIMIT 10) recent)
  )
)
").bind(to_db(guild)?).bind(days).bind(offset).fetch_one(&self.pool).await?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    #[sqlx::test(migrations = "./migrations")]
    async fn stats_separate_guilds_bots_and_days(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let guild = Id::new(1);
        db.record_member_activity(guild, true).await.unwrap();
        db.record_member_activity(guild, true).await.unwrap();
        db.record_member_activity(guild, false).await.unwrap();
        db.record_member_activity(Id::new(2), true).await.unwrap();
        sqlx::query("INSERT INTO saved_messages (guild_id,message_id,channel_id,author_id,author_bot,content,embed_count,sticker_count,mentions_everyone,created_at) VALUES (1,1,1,1,false,'secret',0,0,false,now()), (1,2,1,1,true,'bot',0,0,false,now()), (2,3,1,1,false,'other',0,0,false,now()), (1,4,1,1,false,'old',0,0,false,now()-interval '40 days')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO cases (guild_id,number,module,kind,target,moderator,reason,created_at,revoked_at) VALUES (1,1,'moderation','warn',2,3,'',now(),now()), (1,2,'moderation','ban',2,3,NULL,now()-interval '40 days',NULL), (2,1,'moderation','warn',2,3,NULL,now(),NULL)").execute(&pool).await.unwrap();
        let stats = db.guild_stats(guild, 7, 0).await.unwrap();
        assert_eq!(stats["members"]["joins"], 2);
        assert_eq!(stats["members"]["leaves"], 1);
        assert_eq!(stats["members"]["by_day"].as_array().unwrap().len(), 7);
        assert!(stats["members"]["by_day"][0]["joins"].is_null());
        assert_eq!(stats["messages"]["total"], 1);
        assert_eq!(stats["cases"]["total"], 1);
        assert_eq!(stats["cases"]["recent"].as_array().unwrap().len(), 2);
        assert_eq!(stats["cases"]["recent"][0]["revoked"], true);
        assert_eq!(stats["cases"]["recent"][0]["reason"], "");
        assert!(!stats.to_string().contains("secret"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stats_use_local_day_and_mark_days_before_recording(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        sqlx::query("INSERT INTO guild_member_activity (guild_id,hour,joins) VALUES (1,date_trunc('day',now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - interval '2 days' + interval '18 hours',1)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO saved_messages (guild_id,message_id,channel_id,author_id,author_bot,content,embed_count,sticker_count,mentions_everyone,created_at) VALUES (1,1,1,1,false,'',0,0,false,date_trunc('day',now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - interval '2 days' + interval '18 hours')").execute(&pool).await.unwrap();
        let utc = db.guild_stats(Id::new(1), 7, 0).await.unwrap();
        let tokyo = db.guild_stats(Id::new(1), 7, 9 * 3600).await.unwrap();
        for key in ["members", "messages"] {
            let field = if key == "members" { "joins" } else { "count" };
            let u = utc[key]["by_day"].as_array().unwrap();
            let t = tokyo[key]["by_day"].as_array().unwrap();
            assert!(u[0][field].is_null());
            assert!(t[0][field].is_null());
            let uday = u.iter().find(|day| day[field] == 1).unwrap()["date"]
                .as_str()
                .unwrap();
            let tday = t.iter().find(|day| day[field] == 1).unwrap()["date"]
                .as_str()
                .unwrap();
            assert_ne!(uday, tday);
            assert!(uday < tday);
        }
    }
}
