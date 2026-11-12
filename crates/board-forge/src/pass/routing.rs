//! Opt-in largest-headroom routing; no reads or dispatch authority of its own.
use super::*;

/// Reserve one percentage point per start until the next pass reads actual usage.
/// This is a scheduling estimate, not a measured cost or an additional allowance.
const START_POINTS: f64 = 1.0;

#[derive(Default)]
pub(super) struct Routing {
    starts: BTreeMap<String, u64>,
    choices: BTreeMap<String, String>,
    pub lines: Vec<String>,
}

impl Routing {
    pub fn choose(&mut self, rooms: Vec<(String, f64)>) -> Result<String, String> {
        let mut rooms: BTreeMap<String, f64> = rooms.into_iter().collect();
        for (name, room) in &mut rooms {
            *room -= self.starts.get(name).copied().unwrap_or(0) as f64 * START_POINTS;
        }
        let mut rooms: Vec<_> = rooms
            .into_iter()
            .filter(|(_, room)| room.is_finite() && *room > 0.0)
            .collect();
        rooms.sort_by(|(a, ar), (b, br)| br.total_cmp(ar).then_with(|| a.cmp(b)));
        let Some((pool, room)) = rooms.first() else {
            return Err("no allowed pool has pace headroom after this pass's starts".into());
        };
        let mut line = format!("pool: {pool} (headroom {room:.0}%)");
        for (other, room) in rooms.iter().skip(1) {
            line.push_str(&format!(" over {other} ({room:.0}%)"));
        }
        self.choices.insert(pool.clone(), line);
        Ok(pool.clone())
    }

    pub fn started(&mut self, pool: &str) {
        let account = mix::account(pool).unwrap_or(pool);
        let line = self.choices.remove(pool).unwrap_or_else(|| {
            format!("pool: {pool} (explicit pool or mix override; paced by {account})")
        });
        self.lines.push(line);
        let count = self.starts.entry(account.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_headroom_until_even_regardless_of_order() {
        for reverse in [false, true] {
            let mut routing = Routing::default();
            let candidates = || {
                let mut rooms = vec![("a".into(), 52.0 - 1.0), ("b".into(), 52.0 - 40.0)];
                if reverse {
                    rooms.reverse();
                }
                rooms
            };
            for _ in 0..39 {
                let pool = routing.choose(candidates()).unwrap();
                assert_eq!(pool, "a");
                routing.started(&pool);
            }
            assert_eq!(routing.lines[0], "pool: a (headroom 51%) over b (12%)");
            // At equal headroom, name order breaks ties, not config order.
            assert_eq!(routing.choose(candidates()).unwrap(), "a");
            routing.started("a");
            assert_eq!(routing.choose(candidates()).unwrap(), "b");
            routing.started("b");
            assert_eq!(routing.choose(candidates()).unwrap(), "a");
        }
    }

    #[test]
    fn skips_unreadable_or_spent_room_and_charges_variant_account() {
        let mut routing = Routing::default();
        let rooms = || {
            vec![
                ("claude".into(), 1.0),
                ("openai".into(), 0.5),
                ("bad".into(), f64::NAN),
                ("infinite".into(), f64::INFINITY),
                ("at-target".into(), 0.0),
                ("over".into(), -1.0),
            ]
        };
        assert_eq!(routing.choose(rooms()).unwrap(), "claude");
        // Selection alone spends nothing; a held/capped candidate is not a start.
        assert_eq!(routing.choose(rooms()).unwrap(), "claude");
        routing.started("fable");
        assert_eq!(routing.choose(rooms()).unwrap(), "openai");
        routing.started("astra");
        assert!(routing.choose(rooms()).is_err());
    }
}
