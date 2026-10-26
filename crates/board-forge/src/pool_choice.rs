//! One uniform draw after callers have applied their eligibility rules.

/// Draw without modulo bias. Entropy failure (including repeatedly rejected
/// draws) waits rather than silently favoring the first configured pool.
pub(crate) fn index(len: usize) -> Result<usize, String> {
    #[cfg(test)]
    if let Some(index) = TEST_INDEX.get() {
        return Ok(index.min(len.saturating_sub(1)));
    }
    const DRAW_LIMIT: usize = 128;
    if len <= 1 {
        return Ok(0);
    }
    let len = len as u64;
    let threshold = len.wrapping_neg() % len;
    for _ in 0..DRAW_LIMIT {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).map_err(|e| format!("pool choice entropy unavailable: {e}"))?;
        let value = u64::from_le_bytes(bytes);
        if value >= threshold {
            return Ok((value % len) as usize);
        }
    }
    Err("pool choice entropy draw limit reached".into())
}

#[cfg(test)]
thread_local! {
    static TEST_INDEX: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Scoped, thread-local injection for full dispatch replays as well as unit
/// selectors. Restores the source on panic and does not affect parallel tests.
#[cfg(test)]
pub(crate) fn with_index<T>(index: usize, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<usize>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_INDEX.set(self.0);
        }
    }
    let _reset = Reset(TEST_INDEX.replace(Some(index)));
    run()
}

/// Stable deduplication makes each eligible pool one ticket, not each mention.
/// The injected index source sees only the final eligible set.
pub(crate) fn choose(
    candidates: Vec<String>,
    mut draw: impl FnMut(usize) -> Result<usize, String>,
) -> Result<String, String> {
    let mut unique = Vec::new();
    for name in candidates {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    if unique.is_empty() {
        return Err("no eligible pool".into());
    }
    let index = if unique.len() == 1 {
        0
    } else {
        draw(unique.len())?
    };
    unique
        .get(index)
        .cloned()
        .ok_or_else(|| "pool choice index out of range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_unique_pool_has_one_ticket() {
        for (index, expected) in [(0, "claude"), (1, "openai")] {
            let names = ["claude", "openai", "claude"].map(str::to_owned).to_vec();
            assert_eq!(
                choose(names, |len| {
                    assert_eq!(len, 2);
                    Ok(index)
                }),
                Ok(expected.into())
            );
        }
        assert_eq!(
            choose(vec!["openai".into()], |_| panic!("sole pool needs no draw")),
            Ok("openai".into())
        );
        assert!(
            choose(vec!["claude".into(), "openai".into()], |_| Err(
                "entropy unavailable".into()
            ))
            .is_err()
        );
    }
}
