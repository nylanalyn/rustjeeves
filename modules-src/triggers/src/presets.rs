//! Built-in refrains, carried over from the retired banter module.

use crate::Trigger;

const SAILING_LINES: &[&str] = &[
    "A touch of weather helm is conversation, {user}; an armful is merely drag.",
    "The leeward telltales have rendered their verdict, {user}: ease a fraction and let them fly.",
    "Reef while it is still a tactical decision, {user}, not an athletic emergency.",
    "Velocity made good is the honest measure, {user}; pointing prettily is not the same as arriving.",
    "In the gust, traveler down before mainsheet out, {user}; preserve the leech before surrendering shape.",
    "The apparent wind always creeps forward as the boat accelerates, {user}. Trim for the wind you have made.",
    "A clean bottom and a quiet helm win arguments long before the start gun, {user}.",
    "Keep the slot breathing, {user}; a strangled leeward side helps neither main nor headsail.",
    "The sea has accepted your sail plan, {user}, subject to the usual amendments in wind and judgment.",
    "A fair lead, a fair line, and no unnecessary turns around the winch, {user}. Civilization endures.",
    "Tension the halyard for the luff you need, {user}; wrinkles are instruments, not decorations.",
    "Downwind, sail the pressure rather than the compass, {user}; the shortest line is often the slow one.",
    "The vang is attending to twist, {user}; one trusts the boom will now behave like a gentleman.",
    "Current is a moving racecourse, {user}. Laylines drawn on the land are works of fiction.",
    "When in doubt, {user}, make the boat fast before attempting to make it clever.",
    "The favored tack is temporary, {user}; pressure and shift remain the more durable acquaintances.",
    "A smooth tack begins before the helm moves, {user}: speed first, turn second, trim throughout.",
    "One hand for the vessel and one for yourself, {user}; the sea is unimpressed by misplaced confidence.",
    "The compass says header, the water says current, and the telltales say trim, {user}. Hear all three.",
    "There is no shame in easing six millimetres, {user}. There is considerable shame in stalling the foil.",
];
const CROW_LINES: &[&str] = &[
    "The murder hears you, {user}. The murder hears, and requests shiny things.",
    "A black feather has been placed beside your name in the ledger, {user}. This is probably favorable.",
    "The crows acknowledge your call, {user}. Their reply is delayed by committee.",
    "Three crows have convened on the eastern wire, {user}. None will disclose the agenda.",
    "Your message has entered the rookery, {user}. Expect judgment at dusk.",
    "The eldest crow tilts its head, {user}. You have either impressed it or incurred a small debt.",
    "A distant wingbeat answers, {user}. The murder is awake now.",
    "The crows repeat your name softly, {user}, testing how it sounds in prophecy.",
    "One crow brings a button, another a warning, {user}. You may choose only one.",
    "The rooftop parliament recognizes the delegate from {user}.",
    "A crow has added your call to the old songs, {user}. The rhyme is ominously good.",
    "The murder approves, {user}, though the minutes will record several tasteful objections.",
    "Something black-winged has carried your words beyond the treeline, {user}.",
    "The crows know what you meant, {user}. Regrettably, they also know what you did not say.",
    "A walnut has been left at the threshold for you, {user}. Crow diplomacy proceeds apace.",
    "The western murder answers in kind, {user}. The eastern murder claims prior art.",
    "Seven bright eyes turn toward you, {user}. The eighth is watching something behind you.",
    "Your call was acceptable, {user}. The crows will permit the sun to set on schedule.",
    "The rookery stirs, {user}. Somewhere, a small and ceremonial key has changed hands.",
    "The murder has heard your petition, {user}. Tribute may be paid in peanuts or secrets.",
];

/// A fresh copy of a named preset. The sailing preset still needs its sailor's nick.
pub(crate) fn trigger(name: &str) -> Option<Trigger> {
    let (phrases, lines, cooldown_seconds): (&[&str], &[&str], i64) = match name {
        "crows" | "crow" => (&["caw", "kaw"], CROW_LINES, 8),
        "sailing" | "sail" => (&["sail"], SAILING_LINES, 15),
        _ => return None,
    };
    Some(Trigger {
        phrases: phrases.iter().map(|phrase| (*phrase).to_string()).collect(),
        responses: lines.iter().map(|line| (*line).to_string()).collect(),
        cooldown_seconds,
        ..Default::default()
    })
}
