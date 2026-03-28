use super::AudioCache;
use async_trait::async_trait;
use color_eyre::Result;
use redis::AsyncCommands;
use redis::Client;
use redis::aio::MultiplexedConnection;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RedisCache {
    conn: MultiplexedConnection,
}

impl RedisCache {
    pub async fn new(redis_url: &str) -> Result<Self> {
        let client = Client::open(redis_url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(RedisCache { conn })
    }
}

#[async_trait]
impl AudioCache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut conn = self.conn.clone();
        let data: Option<Vec<u8>> = conn.get(key).await?;
        Ok(data)
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<()> {
        let mut conn = self.conn.clone();
        let res = match ttl {
            Some(duration) => conn.set_ex(key, value, duration.as_secs()).await,
            None => conn.set(key, value).await,
        };

        res.map_err(Into::into)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        conn.del(key).await.map_err(Into::into)
    }
}
