//! Prisoners and ransoms — the one place in the game where crew and gold change hands on a
//! promise made earlier. Every rule here is pure over the game tree so the exchange can be
//! unit-tested off-wasm; [`crate::pm`] owns the messaging around it.
//!
//! The invariant the whole module exists to protect: **a ransom is only ever honoured while the
//! holder still has the exact prisoner group it was written against.** Marooning or press-ganging
//! that group voids the offer instead of minting crew out of nothing.

use crate::model::{Game, Prisoner, Ransom, MAX_RANSOMS};
use crate::Rng;

/// Largest ransom a captain may name.
const MAX_RANSOM_GOLD: i64 = 100_000;
/// Defensive bound on one prisoner group's coin flips.
const MAX_GROUP: i64 = 1_000;

/// The prisoner group a ransom offer covers, if it is still held. Offers written before prisoner
/// ids were recorded (`prisoner_id == 0`) fall back to the holder/origin pair they were made with.
pub(crate) fn ransom_prisoner<'a>(game: &'a Game, ransom: &Ransom) -> Option<&'a Prisoner> {
    game.prisoners.iter().find(|prisoner| {
        if ransom.prisoner_id != 0 {
            prisoner.id == ransom.prisoner_id
        } else {
            prisoner.holder_uuid == ransom.holder_uuid && prisoner.origin_uuid == ransom.target_uuid
        }
    })
}

/// Why a ransom could not be offered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OfferError {
    NoPrisoners,
    /// Zero, negative, or above [`MAX_RANSOM_GOLD`].
    BadAmount,
    /// The game is already holding [`MAX_RANSOMS`] offers.
    NoSpace,
    /// This captain has already named a price for that group.
    Duplicate,
}

/// A ransom the holder just posted, for the caller to announce.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    pub(crate) target_uuid: String,
    pub(crate) target_nick: String,
    pub(crate) count: i64,
}

/// Offer the first prisoner group `holder` is keeping back to the captain it was taken from.
pub(crate) fn offer_ransom(
    game: &mut Game,
    holder: &str,
    amount: i64,
    id: u64,
    now: i64,
) -> Result<Offer, OfferError> {
    let Some(prisoner) = game
        .prisoners
        .iter()
        .find(|prisoner| prisoner.holder_uuid == holder)
        .cloned()
    else {
        return Err(OfferError::NoPrisoners);
    };
    if amount <= 0 || amount > MAX_RANSOM_GOLD {
        return Err(OfferError::BadAmount);
    }
    if game.ransoms.len() >= MAX_RANSOMS {
        return Err(OfferError::NoSpace);
    }
    if game
        .ransoms
        .iter()
        .any(|ransom| ransom.prisoner_id == prisoner.id)
    {
        return Err(OfferError::Duplicate);
    }
    let target_nick = game
        .players
        .get(&prisoner.origin_uuid)
        .map(|player| player.nick_cache.clone())
        .unwrap_or_default();
    game.ransoms.push(Ransom {
        id,
        prisoner_id: prisoner.id,
        holder_uuid: holder.to_string(),
        target_uuid: prisoner.origin_uuid.clone(),
        amount,
        count: prisoner.count,
        offered_at: now,
    });
    Ok(Offer {
        target_uuid: prisoner.origin_uuid,
        target_nick,
        count: prisoner.count,
    })
}

/// What happened when a captain disposed of everyone they were holding.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Release {
    /// Prisoners disposed of, across every group.
    pub(crate) total: i64,
    /// Of those, how many joined the holder's crew (press-gang only).
    pub(crate) pressed: i64,
}

/// Maroon or press-gang every prisoner `holder` is keeping. Press-ganged prisoners flip a coin
/// each: heads they join the holder's crew, tails they slip home to their own captain. Either way
/// the group is gone, so any ransom the holder named against it is torn up.
///
/// Returns `None` when the captain holds nobody.
pub(crate) fn release_prisoners(
    game: &mut Game,
    holder: &str,
    maroon: bool,
    notoriety_per: i64,
    rng: &mut Rng,
) -> Option<Release> {
    let held = game
        .prisoners
        .iter()
        .filter(|prisoner| prisoner.holder_uuid == holder)
        .cloned()
        .collect::<Vec<_>>();
    if held.is_empty() {
        return None;
    }
    game.prisoners
        .retain(|prisoner| prisoner.holder_uuid != holder);
    game.ransoms.retain(|ransom| ransom.holder_uuid != holder);

    let total: i64 = held.iter().map(|prisoner| prisoner.count.max(0)).sum();
    let mut pressed = 0i64;
    if maroon {
        if let Some(player) = game.players.get_mut(holder) {
            player.notoriety += notoriety_per * total;
            player.career_prisoners_marooned += total;
        }
    } else {
        for prisoner in held {
            // Escapees are tallied per group so they sail home to their own captain.
            let mut escaped = 0i64;
            for _ in 0..prisoner.count.clamp(0, MAX_GROUP) {
                if rng.chance(0.5) {
                    pressed += 1;
                } else {
                    escaped += 1;
                }
            }
            if let Some(origin) = game.players.get_mut(&prisoner.origin_uuid) {
                origin.crew_regular += escaped;
            }
        }
        if let Some(player) = game.players.get_mut(holder) {
            player.crew_regular += pressed;
        }
    }
    Some(Release { total, pressed })
}

/// The result of trying to buy your crew back.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Payment {
    /// Nobody is holding this captain to ransom.
    NoOffer,
    /// The offer outlived the prisoners it was written against; it has been withdrawn.
    Stale,
    /// Not enough gold on hand.
    Short {
        amount: i64,
    },
    Paid {
        freed: i64,
        amount: i64,
    },
}

/// Pay the ransom awaiting `payer`: gold to the holder, crew home, prisoners and offer cleared.
pub(crate) fn pay_ransom(game: &mut Game, payer: &str) -> Payment {
    let Some(index) = game
        .ransoms
        .iter()
        .position(|ransom| ransom.target_uuid == payer)
    else {
        return Payment::NoOffer;
    };
    let ransom = game.ransoms[index].clone();
    let Some((prisoner_id, freed)) =
        ransom_prisoner(game, &ransom).map(|prisoner| (prisoner.id, prisoner.count.max(0)))
    else {
        game.ransoms.remove(index);
        return Payment::Stale;
    };
    let Some(player) = game.players.get_mut(payer) else {
        return Payment::NoOffer;
    };
    if player.gold < ransom.amount {
        return Payment::Short {
            amount: ransom.amount,
        };
    }
    player.gold -= ransom.amount;
    player.crew_regular += freed;
    if let Some(holder) = game.players.get_mut(&ransom.holder_uuid) {
        holder.gold += ransom.amount;
    }
    game.prisoners.retain(|prisoner| prisoner.id != prisoner_id);
    game.ransoms.remove(index);
    Payment::Paid {
        freed,
        amount: ransom.amount,
    }
}

/// Leave your crew to the sharks: the offer is torn up (the holder keeps the prisoners and may
/// still maroon or press-gang them) and the abandoning captain loses a point of Notoriety.
///
/// Shares [`Payment`] with [`pay_ransom`] so callers can treat both replies alike: `Paid` with
/// nothing freed, because the crew were written off rather than bought back.
pub(crate) fn abandon_ransom(game: &mut Game, payer: &str) -> Payment {
    let Some(index) = game
        .ransoms
        .iter()
        .position(|ransom| ransom.target_uuid == payer)
    else {
        return Payment::NoOffer;
    };
    game.ransoms.remove(index);
    if let Some(player) = game.players.get_mut(payer) {
        player.notoriety -= 1;
    }
    Payment::Paid {
        freed: 0,
        amount: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Player;

    /// Bob holds 3 of Alice's crew as prisoner group 10.
    fn captured() -> Game {
        let mut game = Game::default();
        game.players.insert(
            "alice".into(),
            Player {
                nick_cache: "Alice".into(),
                gold: 1_000,
                crew_regular: 4,
                ..Default::default()
            },
        );
        game.players.insert(
            "bob".into(),
            Player {
                nick_cache: "Bob".into(),
                gold: 100,
                crew_regular: 5,
                ..Default::default()
            },
        );
        game.prisoners.push(Prisoner {
            id: 10,
            holder_uuid: "bob".into(),
            origin_uuid: "alice".into(),
            count: 3,
            captured_at: 0,
        });
        game
    }

    #[test]
    fn paying_a_ransom_moves_gold_one_way_and_crew_the_other() {
        let mut game = captured();
        let offer = offer_ransom(&mut game, "bob", 500, 1, 0).unwrap();
        assert_eq!(offer.target_nick, "Alice");
        assert_eq!(offer.count, 3);

        assert_eq!(
            pay_ransom(&mut game, "alice"),
            Payment::Paid {
                freed: 3,
                amount: 500
            }
        );
        assert_eq!(game.players["alice"].gold, 500);
        assert_eq!(game.players["alice"].crew_regular, 7, "3 crew came home");
        assert_eq!(game.players["bob"].gold, 600);
        assert!(game.prisoners.is_empty() && game.ransoms.is_empty());
        assert_eq!(pay_ransom(&mut game, "alice"), Payment::NoOffer);
    }

    #[test]
    fn marooning_the_prisoners_voids_the_ransom_instead_of_minting_crew() {
        let mut game = captured();
        offer_ransom(&mut game, "bob", 5_000, 1, 0).unwrap();
        let release = release_prisoners(&mut game, "bob", true, 3, &mut Rng::new(1)).unwrap();
        assert_eq!(release.total, 3);
        assert!(game.ransoms.is_empty(), "the offer died with the prisoners");

        // Alice cannot be charged for crew that no longer exist.
        assert_eq!(pay_ransom(&mut game, "alice"), Payment::NoOffer);
        assert_eq!(game.players["alice"].gold, 1_000);
        assert_eq!(game.players["alice"].crew_regular, 4);
        assert_eq!(game.players["bob"].gold, 100);
    }

    #[test]
    fn a_stale_offer_is_withdrawn_rather_than_honoured() {
        // An offer whose prisoner group vanished by some other route (season boundary, data
        // deletion) must not pay out.
        let mut game = captured();
        offer_ransom(&mut game, "bob", 500, 1, 0).unwrap();
        game.prisoners.clear();
        assert_eq!(pay_ransom(&mut game, "alice"), Payment::Stale);
        assert_eq!(game.players["alice"].gold, 1_000, "no gold changed hands");
        assert_eq!(game.players["alice"].crew_regular, 4, "no crew appeared");
        assert!(game.ransoms.is_empty(), "and the offer is gone");
    }

    #[test]
    fn paying_frees_only_the_group_the_offer_named() {
        let mut game = captured();
        game.prisoners.push(Prisoner {
            id: 11,
            holder_uuid: "bob".into(),
            origin_uuid: "alice".into(),
            count: 4,
            captured_at: 0,
        });
        offer_ransom(&mut game, "bob", 100, 1, 0).unwrap();
        assert_eq!(
            pay_ransom(&mut game, "alice"),
            Payment::Paid {
                freed: 3,
                amount: 100
            }
        );
        assert_eq!(game.players["alice"].crew_regular, 7, "only group 10 freed");
        assert_eq!(game.prisoners.len(), 1, "group 11 is still held");
        assert_eq!(game.prisoners[0].id, 11);
    }

    #[test]
    fn press_ganged_escapees_return_to_their_own_captain() {
        let mut game = captured();
        game.players.insert(
            "carol".into(),
            Player {
                nick_cache: "Carol".into(),
                crew_regular: 0,
                ..Default::default()
            },
        );
        game.prisoners.push(Prisoner {
            id: 11,
            holder_uuid: "bob".into(),
            origin_uuid: "carol".into(),
            count: 4,
            captured_at: 0,
        });
        let release = release_prisoners(&mut game, "bob", false, 3, &mut Rng::new(4)).unwrap();
        assert_eq!(release.total, 7);

        let alice_home = game.players["alice"].crew_regular - 4;
        let carol_home = game.players["carol"].crew_regular;
        assert!(
            (0..=3).contains(&alice_home),
            "Alice's 3 at most: {alice_home}"
        );
        assert!(
            (0..=4).contains(&carol_home),
            "Carol's 4 at most: {carol_home}"
        );
        assert_eq!(
            release.pressed + alice_home + carol_home,
            7,
            "every prisoner is either pressed or home — none duplicated or lost"
        );
        assert_eq!(game.players["bob"].crew_regular, 5 + release.pressed);
    }

    #[test]
    fn escapees_of_a_departed_captain_are_not_credited_to_the_next_group() {
        // Alice has left the game; her escapees must simply vanish, not land in Carol's crew.
        let mut game = captured();
        game.players.remove("alice");
        game.players.insert(
            "carol".into(),
            Player {
                nick_cache: "Carol".into(),
                crew_regular: 0,
                ..Default::default()
            },
        );
        game.prisoners.push(Prisoner {
            id: 11,
            holder_uuid: "bob".into(),
            origin_uuid: "carol".into(),
            count: 2,
            captured_at: 0,
        });
        release_prisoners(&mut game, "bob", false, 3, &mut Rng::new(2)).unwrap();
        assert!(
            game.players["carol"].crew_regular <= 2,
            "Carol can only get her own 2 back, never Alice's 3"
        );
    }

    #[test]
    fn offers_are_validated_and_cannot_be_stacked_on_one_group() {
        let mut game = captured();
        assert_eq!(
            offer_ransom(&mut game, "alice", 100, 1, 0),
            Err(OfferError::NoPrisoners)
        );
        assert_eq!(
            offer_ransom(&mut game, "bob", 0, 1, 0),
            Err(OfferError::BadAmount)
        );
        assert_eq!(
            offer_ransom(&mut game, "bob", MAX_RANSOM_GOLD + 1, 1, 0),
            Err(OfferError::BadAmount)
        );
        offer_ransom(&mut game, "bob", 500, 1, 0).unwrap();
        assert_eq!(
            offer_ransom(&mut game, "bob", 900, 2, 0),
            Err(OfferError::Duplicate),
            "no re-pricing the same prisoners into a second offer"
        );
        assert_eq!(game.ransoms.len(), 1);
    }

    #[test]
    fn legacy_offers_without_a_prisoner_id_still_resolve() {
        let mut game = captured();
        game.ransoms.push(Ransom {
            id: 1,
            prisoner_id: 0, // written before the id was recorded
            holder_uuid: "bob".into(),
            target_uuid: "alice".into(),
            amount: 200,
            count: 3,
            offered_at: 0,
        });
        assert_eq!(
            pay_ransom(&mut game, "alice"),
            Payment::Paid {
                freed: 3,
                amount: 200
            }
        );
        assert!(game.prisoners.is_empty());
    }

    #[test]
    fn abandoning_leaves_the_prisoners_with_the_holder() {
        let mut game = captured();
        offer_ransom(&mut game, "bob", 500, 1, 0).unwrap();
        assert_eq!(
            abandon_ransom(&mut game, "alice"),
            Payment::Paid {
                freed: 0,
                amount: 0
            }
        );
        assert_eq!(game.players["alice"].notoriety, -1);
        assert!(game.ransoms.is_empty());
        assert_eq!(game.prisoners.len(), 1, "Bob keeps them");
        assert_eq!(abandon_ransom(&mut game, "alice"), Payment::NoOffer);
    }

    #[test]
    fn paying_without_the_gold_changes_nothing() {
        let mut game = captured();
        offer_ransom(&mut game, "bob", 5_000, 1, 0).unwrap();
        assert_eq!(
            pay_ransom(&mut game, "alice"),
            Payment::Short { amount: 5_000 }
        );
        assert_eq!(game.players["alice"].gold, 1_000);
        assert_eq!(game.prisoners.len(), 1, "the offer stands");
        assert_eq!(game.ransoms.len(), 1);
    }
}
