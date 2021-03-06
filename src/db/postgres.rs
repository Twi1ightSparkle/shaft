use chrono;
use chrono::TimeZone;
use futures::compat::Future01CompatExt;
use futures::future::LocalBoxFuture;
use futures::{Future, FutureExt};
use futures_cpupool::CpuPool;
use linear_map::LinearMap;
use r2d2;
use r2d2_postgres::PostgresConnectionManager;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use snafu::ResultExt;

use std::pin::Pin;
use std::sync::Arc;

use crate::db::{ConnectionPoolError, Database, DatabaseError, PostgresError, Transaction, User};

/// An implementation of [Database] using posgtres
///
/// Safe to clone as the thread and connection pools will be shared.
#[derive(Clone)]
pub struct PostgresDatabase {
    /// Thread pool used to do database operations.
    cpu_pool: CpuPool,
    /// SQLite connection pool.
    db_pool: Arc<r2d2::Pool<PostgresConnectionManager>>,
}

impl PostgresDatabase {
    /// Create new instance with given path. If file does not exist a new
    /// database is created.
    pub fn with_manager(manager: PostgresConnectionManager) -> PostgresDatabase {
        let pool = r2d2::Pool::new(manager).unwrap();

        PostgresDatabase {
            cpu_pool: CpuPool::new_num_cpus(),
            db_pool: Arc::new(pool),
        }
    }
}

impl Database for PostgresDatabase {
    fn get_user_by_github_id(
        &self,
        github_user_id: String,
    ) -> LocalBoxFuture<'static, Result<Option<String>, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let user_id = conn
                    .query(
                        "SELECT user_id FROM github_users WHERE github_id = $1",
                        &[&github_user_id],
                    )
                    .context(PostgresError)?
                    .iter()
                    .next()
                    .map(|row| row.get(0));

                Ok(user_id)
            })
            .compat()
            .boxed()
    }

    fn add_user_by_github_id(
        &self,
        github_user_id: String,
        display_name: String,
    ) -> LocalBoxFuture<'static, Result<String, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                conn.execute(
                    "INSERT INTO github_users (user_id, github_id)
                VALUES ($1, $1)",
                    &[&github_user_id],
                )
                .context(PostgresError)?;

                conn.execute(
                    "INSERT INTO users (user_id, display_name)
                VALUES ($1, $2)",
                    &[&github_user_id, &display_name],
                )
                .context(PostgresError)?;

                Ok(github_user_id)
            })
            .compat()
            .boxed()
    }

    fn create_token_for_user(
        &self,
        user_id: String,
    ) -> LocalBoxFuture<'static, Result<String, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let token: String = thread_rng().sample_iter(&Alphanumeric).take(32).collect();

                conn.execute(
                    "INSERT INTO tokens (user_id, token) VALUES ($1, $2)",
                    &[&user_id, &token],
                )
                .context(PostgresError)?;

                Ok(token)
            })
            .compat()
            .boxed()
    }

    fn delete_token(&self, token: String) -> LocalBoxFuture<'static, Result<(), DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                conn.execute("DELETE FROM tokens WHERE token = $1", &[&token])
                    .context(PostgresError)?;

                Ok(())
            })
            .compat()
            .boxed()
    }

    fn get_user_from_token(
        &self,
        token: String,
    ) -> LocalBoxFuture<'static, Result<Option<User>, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let row = conn
                    .query(
                        r#"
                    SELECT user_id, display_name, COALESCE(balance, 0)
                    FROM tokens
                    INNER JOIN users USING (user_id)
                    LEFT JOIN (
                        SELECT user_id, SUM(amount) as balance
                        FROM (
                            SELECT shafter AS user_id, SUM(amount) AS amount
                            FROM transactions GROUP BY shafter
                            UNION ALL
                            SELECT shaftee AS user_id, -SUM(amount) AS amount
                            FROM transactions GROUP BY shaftee
                        ) t GROUP BY user_id
                    )
                    USING (user_id)
                    WHERE token = $1
                    "#,
                        &[&token],
                    )
                    .context(PostgresError)?
                    .iter()
                    .next()
                    .map(|row| User {
                        user_id: row.get(0),
                        display_name: row.get(1),
                        balance: row.get(2),
                    });

                Ok(row)
            })
            .compat()
            .boxed()
    }

    fn get_balance_for_user(
        &self,
        user: String,
    ) -> LocalBoxFuture<'static, Result<i64, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                conn.query(
                    r#"SELECT (
                    SELECT COALESCE(SUM(amount), 0)
                        FROM transactions
                        WHERE shafter = $1
                    ) - (
                        SELECT COALESCE(SUM(amount), 0)
                        FROM transactions
                        WHERE shaftee = $1
                    )"#,
                    &[&user],
                )
                .context(PostgresError)?
                .iter()
                .next()
                .map(|row| row.get(0))
                .ok_or_else(|| DatabaseError::UnknownUser { user_id: user })
            })
            .compat()
            .boxed()
    }

    fn get_all_users(
        &self,
    ) -> LocalBoxFuture<'static, Result<LinearMap<String, User>, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let rows: LinearMap<String, User> = conn
                    .query(
                        r#"
                    SELECT user_id, display_name, COALESCE(balance, 0) AS balance
                    FROM users
                    LEFT JOIN (
                        SELECT user_id, SUM(amount) as balance
                        FROM (
                            SELECT shafter AS user_id, SUM(amount) AS amount
                            FROM transactions GROUP BY shafter
                            UNION ALL
                            SELECT shaftee AS user_id, -SUM(amount) AS amount
                            FROM transactions GROUP BY shaftee
                        ) t GROUP BY user_id
                    )
                    USING (user_id)
                    ORDER BY balance ASC
                    "#,
                        &[],
                    )
                    .context(PostgresError)?
                    .iter()
                    .map(|row| {
                        (
                            row.get(0),
                            User {
                                user_id: row.get(0),
                                display_name: row.get(1),
                                balance: row.get(2),
                            },
                        )
                    })
                    .collect();

                Ok(rows)
            })
            .compat()
            .boxed()
    }

    fn shaft_user(
        &self,
        transaction: Transaction,
    ) -> LocalBoxFuture<'static, Result<(), DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let user_exists = conn
                    .query(
                        "SELECT user_id FROM users WHERE user_id = $1",
                        &[&transaction.shaftee],
                    )
                    .context(PostgresError)?
                    .len();

                if user_exists == 0 {
                    return Err(DatabaseError::UnknownUser {
                        user_id: transaction.shaftee,
                    });
                }

                let stmt = conn
                    .prepare(
                        "INSERT INTO transactions (shafter, shaftee, amount, time_sec, reason)\
                     VALUES ($1, $2, $3, $4, $5)",
                    )
                    .context(PostgresError)?;

                stmt.execute(&[
                    &transaction.shafter,
                    &transaction.shaftee,
                    &transaction.amount,
                    &transaction.datetime.timestamp(),
                    &transaction.reason,
                ])
                .context(PostgresError)?;

                Ok(())
            })
            .compat()
            .boxed()
    }

    fn get_last_transactions(
        &self,
        limit: u32,
    ) -> LocalBoxFuture<'static, Result<Vec<Transaction>, DatabaseError>> {
        let db_pool = self.db_pool.clone();

        self.cpu_pool
            .spawn_fn(move || -> Result<_, DatabaseError> {
                let conn = db_pool.get().context(ConnectionPoolError)?;

                let rows: Vec<_> = conn
                    .query(
                        r#"SELECT shafter, shaftee, amount, time_sec, reason
                    FROM transactions
                    ORDER BY id DESC
                    LIMIT $1
                    "#,
                        &[&limit],
                    )
                    .context(PostgresError)?
                    .iter()
                    .map(|row| Transaction {
                        shafter: row.get(0),
                        shaftee: row.get(1),
                        amount: row.get(2),
                        datetime: chrono::Utc.timestamp(row.get(3), 0),
                        reason: row.get(4),
                    })
                    .collect();

                Ok(rows)
            })
            .compat()
            .boxed()
    }
}
