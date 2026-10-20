use extism_pdk::Error;
use jeeves_guest::Entropy;
use serde::{Deserialize, Serialize};

pub const LEAVES: &[&str] = &[
    "rounded",
    "heart-shaped",
    "triangular",
    "feathery",
    "star-shaped",
    "pyramidal",
];
pub const FLOWERS: &[&str] = &[
    "rose-pink",
    "butter-yellow",
    "lavender",
    "crimson",
    "sky-blue",
    "glinting silver",
];
pub const FRUIT: &[&str] = &[
    "delightful",
    "honey-sweet",
    "velvety",
    "fragrant",
    "pearl-dappled",
    "softly luminous",
];
pub const CHARACTER: &[&str] = &[
    "gentle",
    "cheerful",
    "graceful",
    "dreamy",
    "majestic",
    "moon-kissed",
];
pub const STATS: &[&str] = &[
    "planted",
    "watered",
    "matured",
    "harvested",
    "photos",
    "rare",
];
pub const PHOTO_SLOTS: usize = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plant {
    pub leaves: usize,
    pub flowers: usize,
    pub fruit: usize,
    pub character: usize,
    pub waterings: u32,
    pub goal: u32,
    pub interval: i64,
    pub last_bucket: Option<i64>,
}

impl Plant {
    pub fn new(goal: u32, interval: i64, entropy: &mut Entropy) -> Result<Self, Error> {
        // Each trait independently has a 1-in-200 rare variant. No paid rolls or rerolls.
        fn draw(entropy: &mut Entropy) -> Result<usize, Error> {
            if entropy.below(200)? == 0 {
                Ok(5)
            } else {
                Ok(entropy.below(5)? as usize)
            }
        }
        Ok(Self {
            leaves: draw(entropy)?,
            flowers: draw(entropy)?,
            fruit: draw(entropy)?,
            character: draw(entropy)?,
            waterings: 0,
            goal,
            interval,
            last_bucket: None,
        })
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.leaves >= LEAVES.len()
            || self.flowers >= FLOWERS.len()
            || self.fruit >= FRUIT.len()
            || self.character >= CHARACTER.len()
            || !(4..=365).contains(&self.goal)
            || !(60..=604800).contains(&self.interval)
            || self.waterings > self.goal
        {
            return Err(Error::msg("invalid botany plant"));
        }
        Ok(())
    }

    pub fn stage(&self) -> u32 {
        if self.waterings >= self.goal {
            5
        } else {
            (self.waterings.saturating_mul(5) / self.goal).min(4)
        }
    }

    pub fn rare(&self) -> bool {
        [self.leaves, self.flowers, self.fruit, self.character].contains(&5)
    }

    /// Fixed UTC buckets allow a daily visit without a drifting 24-hour deadline.
    /// Backward clock changes cannot grant additional waterings.
    pub fn water(&mut self, now: i64) -> bool {
        let bucket = now.div_euclid(self.interval);
        if self.stage() == 5 || self.last_bucket.is_some_and(|last| bucket <= last) {
            return false;
        }
        self.last_bucket = Some(bucket);
        self.waterings += 1;
        true
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Photo {
    pub plant: Plant,
    pub taken_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GardenBook {
    pub version: u32,
    pub plant: Option<Plant>,
    pub photos: [Option<Photo>; PHOTO_SLOTS],
    pub totals: [u64; 6],
}

impl Default for GardenBook {
    fn default() -> Self {
        Self {
            version: 1,
            plant: None,
            photos: Default::default(),
            totals: [0; 6],
        }
    }
}

impl GardenBook {
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let book: Self = serde_json::from_str(raw)?;
        if book.version != 1 {
            return Err(Error::msg("unsupported botany state version"));
        }
        if let Some(plant) = &book.plant {
            plant.validate()?;
        }
        for photo in book.photos.iter().flatten() {
            photo.plant.validate()?;
        }
        Ok(book)
    }

    pub fn record(&mut self, index: usize) {
        self.totals[index] = self.totals[index].saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plant() -> Plant {
        Plant {
            leaves: 0,
            flowers: 1,
            fruit: 2,
            character: 3,
            waterings: 0,
            goal: 28,
            interval: 86400,
            last_bucket: None,
        }
    }

    #[test]
    fn missed_days_do_not_hurt_or_skip_growth() {
        let mut p = plant();
        assert!(p.water(0));
        assert!(!p.water(86399));
        assert!(p.water(86400 * 100));
        assert_eq!(p.waterings, 2);
        assert!(!p.water(86400));
    }

    #[test]
    fn gradual_reveal_and_indefinite_maturity() {
        let mut p = plant();
        for day in 0..28 {
            assert!(p.water(day * 86400));
            assert_eq!(p.stage(), ((day as u32 + 1) * 5 / 28).min(5));
        }
        let mature = p.clone();
        assert!(!p.water(86400 * 10000));
        assert_eq!(p, mature);
    }

    #[test]
    fn photos_are_snapshots_and_slots_stay_put() {
        let mut book = GardenBook {
            plant: Some(plant()),
            ..Default::default()
        };
        book.photos[4] = Some(Photo {
            plant: book.plant.clone().unwrap(),
            taken_at: 0,
        });
        book.plant.as_mut().unwrap().water(0);
        book.plant = None;
        assert_eq!(book.photos[4].as_ref().unwrap().plant.waterings, 0);
        assert!(book.photos[0].is_none());
        assert_eq!(
            GardenBook::parse(&serde_json::to_string(&book).unwrap()).unwrap(),
            book
        );
    }

    #[test]
    fn malformed_state_is_rejected() {
        let mut book = GardenBook::default();
        let mut p = plant();
        p.leaves = 100;
        book.plant = Some(p);
        assert!(GardenBook::parse(&serde_json::to_string(&book).unwrap()).is_err());
        book.plant = None;
        book.version = 2;
        assert!(GardenBook::parse(&serde_json::to_string(&book).unwrap()).is_err());
    }
}
