use super::AudioCache;
use async_trait::async_trait;
use bytes::Bytes;
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
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let mut conn = self.conn.clone();
        let data: Option<Vec<u8>> = conn.get(key).await?;
        Ok(data.map(Bytes::from))
    }

    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<()> {
        let mut conn = self.conn.clone();
        let res = match ttl {
            Some(duration) => conn.set_ex(key, value.as_ref(), duration.as_secs()).await,
            None => conn.set(key, value.as_ref()).await,
        };

        res.map_err(Into::into)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        conn.del(key).await.map_err(Into::into)
    }
}
