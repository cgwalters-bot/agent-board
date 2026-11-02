//! One draw after callers have applied their eligibility rules: uniform, or
//! weighted by each pool's room under its pace.

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

/// The least weight an eligible pool draws with, in points of room: a pool
/// just past its pace, still within its burst, keeps a small share.
const ROOM_FLOOR: f64 = 1.0;

/// The most weight a pool draws with: a paced window holds 100 points, and
/// the cap keeps a malformed reading from overflowing the ticket count.
const ROOM_CAP: f64 = 100.0;

/// Tickets a point of room buys, so fractional room still counts.
const TICKETS_PER_POINT: f64 = 100.0;

/// Draw among `(pool, room)` in proportion to each pool's room, clamped to
/// [`ROOM_FLOOR`]..=[`ROOM_CAP`]. The first mention of a pool is its weight.
/// The injected source draws a ticket; a test's `with_index` instead names
/// the eligible pool by position, as for [`choose`].
pub(crate) fn choose_weighted(
    candidates: Vec<(String, f64)>,
    mut draw: impl FnMut(usize) -> Result<usize, String>,
) -> Result<String, String> {
    let mut unique: Vec<(String, usize)> = Vec::new();
    for (name, room) in candidates {
        if !unique.iter().any(|(n, _)| *n == name) {
            // A NaN room would clamp to NaN; it draws as the floor.
            let room = match room.is_nan() {
                true => ROOM_FLOOR,
                false => room.clamp(ROOM_FLOOR, ROOM_CAP),
            };
            unique.push((name, (room * TICKETS_PER_POINT).round() as usize));
        }
    }
    #[cfg(test)]
    if let Some(index) = TEST_INDEX.get()
        && !unique.is_empty()
    {
        return Ok(unique[index.min(unique.len() - 1)].0.clone());
    }
    let mut ticket = match unique.len() {
        0 => return Err("no eligible pool".into()),
        1 => 0,
        _ => draw(unique.iter().map(|(_, t)| t).sum())?,
    };
    for (name, tickets) in unique {
        if ticket < tickets {
            return Ok(name);
        }
        ticket -= tickets;
    }
    Err("pool choice ticket out of range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each case's draws are fixed tickets: the last of the first pool's
    /// share and the first past it, giving each pool's share of the total.
    #[test]
    fn weighted_shares_follow_room() {
        for (name, rooms, total, first) in [
            ("about 96% and 4%", [23.0, -3.0], 2400, 2300),
            ("equal room", [10.0, 10.0], 2000, 1000),
            ("no room, the floor", [0.0, -50.0], 200, 100),
            ("fractional room", [0.5, 2.25], 325, 100),
            ("past the cap", [1e9, 100.0], 20000, 10000),
            ("NaN, the floor", [f64::NAN, 1.0], 200, 100),
        ] {
            let candidates = || {
                let named = ["a", "b"].into_iter().map(str::to_owned);
                named.zip(rooms).collect::<Vec<_>>()
            };
            for ticket in [0, first - 1, first, total - 1] {
                let expected = if ticket < first { "a" } else { "b" };
                let got = choose_weighted(candidates(), |len| {
                    assert_eq!(len, total, "{name}");
                    Ok(ticket)
                });
                assert_eq!(got, Ok(expected.into()), "{name}: ticket {ticket}");
            }
        }
        let pools = |rooms: &[(&str, f64)]| {
            let pools = rooms.iter().map(|(n, r)| (n.to_string(), *r));
            pools.collect::<Vec<_>>()
        };
        let duplicated = pools(&[("a", 1.0), ("b", 1.0), ("a", 50.0)]);
        assert_eq!(
            choose_weighted(duplicated, |len| {
                assert_eq!(len, 200);
                Ok(150)
            }),
            Ok("b".into())
        );
        let sole = pools(&[("a", -9.0)]);
        assert_eq!(
            choose_weighted(sole, |_| panic!("sole pool needs no draw")),
            Ok("a".into())
        );
        assert_eq!(
            choose_weighted(vec![], |_| panic!("no pool needs no draw")),
            Err("no eligible pool".into())
        );
        let two = pools(&[("a", 1.0), ("b", 1.0)]);
        assert_eq!(
            choose_weighted(two.clone(), |_| Err("entropy unavailable".into())),
            Err("entropy unavailable".into())
        );
        assert_eq!(
            choose_weighted(two, |_| Ok(200)),
            Err("pool choice ticket out of range".into())
        );
    }

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
