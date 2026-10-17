use std::hash::{BuildHasher, Hasher, RandomState};

const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// `prefix` followed by `chars` random lowercase letters and digits. Not for secrets: ids only
/// need to differ between runs.
pub fn token(prefix: &str, chars: usize) -> String {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    let mut bits = hasher.finish();
    let mut id = String::from(prefix);
    for _ in 0..chars {
        if bits == 0 {
            // A 64-bit hash holds about twelve base-36 digits; draw again for longer ids.
            let mut more = RandomState::new().build_hasher();
            more.write_u32(std::process::id());
            bits = more.finish();
        }
        let index = usize::try_from(bits % 36).unwrap_or(0);
        id.push(char::from(ALPHABET[index]));
        bits /= 36;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_prefix_and_length() {
        let id = token("x-", 8);
        assert!(id.starts_with("x-"));
        assert_eq!(id.len(), 10);
        assert!(id[2..].bytes().all(|b| ALPHABET.contains(&b)));
    }
}
