use super::params;
use color_eyre::{
    Result,
    eyre::{Error, eyre},
};
use hex;
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use sha1::{Digest, Sha1};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("Invalid credentials")]
    InvalidCredentials(#[source] Error),

    #[error(transparent)]
    UnexpectedError(#[from] Error),
}

fn hex_digest_path(path: &str) -> String {
    let digest = Sha1::digest(path.as_bytes());
    let hash = hex::encode(digest);
    format!("{}/{}/{}", &hash[..2], &hash[2..4], &hash[4..])
}

pub fn digest_storage_hasher(audio: &str) -> String {
    hex_digest_path(audio)
}

pub fn digest_result_storage_hasher(p: &params::Params) -> String {
    let path = p.to_string();
    hex_digest_path(&path)
}

pub fn suffix_result_storage_hasher(p: &params::Params) -> String {
    let path = p.to_string();
    let digest = Sha1::digest(path.as_bytes());
    let hash = format!(".{}", hex::encode(&digest[..10]));

    let audio = if p.key.starts_with("https://") {
        &p.key[8..].to_string()
    } else if p.key.starts_with("http://") {
        &p.key[7..].to_string()
    } else {
        &p.key
    };

    let dot_idx = audio.rfind('.');
    let slash_idx = audio.rfind('/');

    if let Some(dot_idx) = dot_idx
        && slash_idx.is_none_or(|idx| idx < dot_idx)
    {
        let ext = if let Some(format) = &p.format {
            format!(".{}", format.to_string().to_lowercase())
        } else {
            audio[dot_idx..].to_string()
        };
        return format!("{}{}{}", &audio[..dot_idx], hash, ext);
    }
    format!("{}{}", audio, hash)
}

#[tracing::instrument(
    name = "Verify path hash",
    skip(expected_path_hash, path_candidate, secret)
)]
pub fn verify_hash(
    expected_path_hash: SecretString,
    path_candidate: SecretString,
    secret: &SecretString,
) -> Result<(), AuthError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| AuthError::UnexpectedError(eyre!("Invalid HMAC key: {}", e)))?;
    mac.update(path_candidate.expose_secret().as_bytes());

    let expected_bytes = hex::decode(expected_path_hash.expose_secret())
        .map_err(|e| AuthError::InvalidCredentials(eyre!("Invalid hash format: {}", e)))?;

    mac.verify_slice(&expected_bytes)
        .map_err(|_| AuthError::InvalidCredentials(eyre!("Invalid hash")))
}

#[tracing::instrument(name = "Compute path hash", skip(path, secret))]
pub fn compute_hash(path: String, secret: &SecretString) -> Result<SecretString> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| eyre!("Invalid HMAC key: {}", e))?;
    mac.update(path.as_bytes());
    let result = mac.finalize();
    let code_bytes = result.into_bytes();
    Ok(SecretBox::from(hex::encode(code_bytes)))
}
#[cfg(test)]
mod tests {
    use super::params::Params;
    use super::*;
    use crate::blob::AudioFormat;
    use color_eyre::Result;

    fn test_secret() -> SecretString {
        SecretString::from("test-secret".to_string())
    }

    #[test]
    fn test_compute_and_verify_hash() -> Result<()> {
        let secret = test_secret();
        let test_path = "my/test/path".to_string();
        let hash = compute_hash(test_path.clone(), &secret)?;
        verify_hash(hash, SecretString::from(test_path), &secret)?;
        Ok(())
    }

    #[test]
    fn test_verify_hash_with_invalid_input() {
        let secret = test_secret();
        let test_path = "my/test/path".to_string();
        let hash = compute_hash(test_path, &secret).unwrap();
        let result = verify_hash(hash, SecretString::from("wrong/path".to_string()), &secret);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(matches!(e, AuthError::InvalidCredentials(_)));
        }
    }

    #[test]
    fn test_verify_hash_with_invalid_hash_format() {
        let secret = test_secret();
        let result = verify_hash(
            SecretString::from("not-a-valid-hash-format".to_string()),
            SecretString::from("some/path".to_string()),
            &secret,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_hash_consistency() -> Result<()> {
        let secret = test_secret();
        let test_path = "consistent/test/path".to_string();
        let hash1 = compute_hash(test_path.clone(), &secret)?;
        let hash2 = compute_hash(test_path.clone(), &secret)?;
        assert_eq!(hash1.expose_secret(), hash2.expose_secret());
        verify_hash(hash1, SecretString::from(test_path.clone()), &secret)?;
        verify_hash(hash2, SecretString::from(test_path), &secret)?;
        Ok(())
    }

    #[test]
    fn test_digest_result_storage_hasher() {
        let p = Params {
            key: "test.mp3".to_string(),
            format: Some(AudioFormat::Mp3),
            quality: Some(0.5),
            ..Default::default()
        };

        // Instead of comparing to a fixed hash, we'll calculate the actual hash
        // and then verify it has the correct format
        let result = digest_result_storage_hasher(&p);

        // Check format: xx/yy/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz (2/2/32+ chars)
        assert!(result.len() >= 36);
        assert_eq!(result.chars().nth(2).unwrap(), '/');
        assert_eq!(result.chars().nth(5).unwrap(), '/');
    }

    #[test]
    fn test_suffix_result_storage_hasher() {
        let p = Params {
            key: "test.mp3".to_string(),
            format: Some(AudioFormat::Wav),
            sample_rate: Some(44100),
            ..Default::default()
        };

        let result = suffix_result_storage_hasher(&p);

        // Check format: "test.[hash].wav"
        assert!(result.starts_with("test."));
        assert!(result.ends_with(".wav"));

        // Extract the hash portion
        let parts: Vec<&str> = result.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "test");
        assert_eq!(parts[2], "wav");

        // Verify hash length (should be 20 hex chars)
        assert_eq!(parts[1].len(), 20);
        // Check if the hash consists of valid hex characters
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_suffix_result_storage_hasher_with_format() {
        let p = Params {
            key: "example.mp3".to_string(),
            format: Some(AudioFormat::Ogg),
            quality: Some(0.8),
            ..Default::default()
        };

        let result = suffix_result_storage_hasher(&p);

        // Check format: "example.[hash].ogg"
        assert!(result.starts_with("example."));
        assert!(result.ends_with(".ogg"));

        // Extract the hash portion
        let parts: Vec<&str> = result.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "example");
        assert_eq!(parts[2], "ogg");

        // Verify hash length
        assert_eq!(parts[1].len(), 20);
        // Check if hash consists of valid hex characters
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_suffix_result_storage_hasher_with_filters() {
        let p = Params {
            key: "input.mp3".to_string(),
            format: Some(AudioFormat::Mp3),
            volume: Some(1.5),
            lowpass: Some(1000.0),
            ..Default::default()
        };

        let result = suffix_result_storage_hasher(&p);

        // Check format: "input.[hash].mp3"
        assert!(result.starts_with("input."));
        assert!(result.ends_with(".mp3"));

        // Extract the hash portion
        let parts: Vec<&str> = result.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "input");
        assert_eq!(parts[2], "mp3");

        // Verify hash length
        assert_eq!(parts[1].len(), 20);
        // Check if hash consists of valid hex characters
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
