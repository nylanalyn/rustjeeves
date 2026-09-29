//! Channel stats on the public website: an index of public channels, a page per channel, a Talk
//! panel for public achievement holders, and a sanitized JSON API.
//!
//! Everything here comes from the snapshots the stats module chooses to publish
//! ([`PublicChannelStats`]); this side only renders them. A channel is shown only while its
//! `stats.enabled` and `stats.public_page` settings are both on, checked live, so switching a page
//! off hides it at once even before the module removes the snapshot. Profile IDs in the snapshots
//! are used to find someone's Talk panel and are never output.

use crate::db::DbHandle;
use crate::publicweb::{escape, json_body, query_param, RouteResponse};
use crate::settings::SharedSettingRegistry;
use jeeves_abi::{PublicChannelStats, PublicTalker, PUBLIC_STATS_VERSION};
use serde::Serialize;

const STATS_MODULE: &str = "stats";

/// The snapshots currently allowed on the site, by network then channel.
pub(crate) fn public_channels(
    db: &DbHandle,
    settings: &SharedSettingRegistry,
) -> anyhow::Result<Vec<PublicChannelStats>> {
    let mut channels = Vec::new();
    for entry in db.kv_list_module_prefix_blocking(STATS_MODULE, "public:")? {
        if entry.value.trim().is_empty() {
            continue;
        }
        let Ok(snapshot) = serde_json::from_str::<PublicChannelStats>(&entry.value) else {
            continue;
        };
        if snapshot.version == PUBLIC_STATS_VERSION
            && is_public(settings, &snapshot.server, &snapshot.channel)
        {
            channels.push(snapshot);
        }
    }
    channels.sort_by(|a, b| {
        (a.server.to_lowercase(), a.channel.to_lowercase())
            .cmp(&(b.server.to_lowercase(), b.channel.to_lowercase()))
    });
    Ok(channels)
}

fn is_public(settings: &SharedSettingRegistry, server: &str, channel: &str) -> bool {
    let settings = settings.lock().unwrap();
    ["enabled", "public_page"].iter().all(|key| {
        settings
            .effective(STATS_MODULE, key, Some(server), Some(channel))
            .as_deref()
            == Some("true")
    })
}

fn find<'a>(
    channels: &'a [PublicChannelStats],
    server: &str,
    channel: &str,
) -> Option<&'a PublicChannelStats> {
    channels
        .iter()
        .find(|snapshot| snapshot.server == server && snapshot.channel == channel)
}

/// Someone's figures in each public channel on their network, for their Talk panel.
pub(crate) fn talk_for(
    channels: &[PublicChannelStats],
    server: &str,
    profile_id: &str,
) -> Vec<TalkLine> {
    channels
        .iter()
        .filter(|snapshot| snapshot.server == server)
        .filter_map(|snapshot| {
            snapshot
                .people
                .iter()
                .find(|person| person.profile_id == profile_id)
                .map(|person| TalkLine::new(&snapshot.channel, person))
        })
        .collect()
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct TalkLine {
    pub(crate) channel: String,
    pub(crate) lines: u64,
    pub(crate) rank: u32,
    pub(crate) streak: u32,
    pub(crate) best_streak: u32,
}

impl TalkLine {
    fn new(channel: &str, person: &PublicTalker) -> Self {
        Self {
            channel: channel.into(),
            lines: person.lines,
            rank: person.rank,
            streak: person.streak,
            best_streak: person.best_streak,
        }
    }
}

/// The Talk panel's HTML, or nothing when there's nothing to show.
pub(crate) fn talk_panel(server: &str, lines: &[TalkLine]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut html = String::from("<section class=\"summary talk\"><h2>Talk</h2><ul>");
    for line in lines {
        html.push_str(&format!(
            "<li><strong>{}</strong> lines in <a href=\"/stats?{}\">{}</a> (#{}) · {}-day streak (best {})</li>",
            grouped(line.lines),
            channel_query(server, &line.channel),
            escape(&line.channel),
            line.rank,
            line.streak,
            line.best_streak,
        ));
    }
    html.push_str("</ul></section>");
    html
}

// ── JSON ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChannelList {
    version: u32,
    channels: Vec<ChannelSummary>,
}

#[derive(Serialize)]
struct ChannelSummary {
    server: String,
    channel: String,
    since: String,
    lines_total: u64,
    lines_week: u64,
    updated_at: i64,
}

/// A snapshot as the public sees it: no profile IDs, no Talk figures.
#[derive(Serialize)]
struct ChannelOut<'a> {
    version: u32,
    server: &'a str,
    channel: &'a str,
    timezone: &'a str,
    updated_at: i64,
    since: &'a str,
    lines_total: u64,
    lines_week: u64,
    heatmap: &'a [Vec<u64>],
    days: &'a [jeeves_abi::PublicDay],
    record_day: Option<&'a jeeves_abi::PublicDay>,
    boards: Vec<BoardOut<'a>>,
    awards: Vec<AwardOut<'a>>,
}

#[derive(Serialize)]
struct BoardOut<'a> {
    period: &'a str,
    entries: Vec<RankOut<'a>>,
}

#[derive(Serialize)]
struct RankOut<'a> {
    rank: usize,
    /// Absent for people who haven't made their achievements public.
    name: Option<&'a str>,
    lines: u64,
}

#[derive(Serialize)]
struct AwardOut<'a> {
    title: &'a str,
    figure: &'a str,
    name: Option<&'a str>,
}

pub(crate) fn list_json(channels: &[PublicChannelStats]) -> anyhow::Result<RouteResponse> {
    json_body(
        &ChannelList {
            version: PUBLIC_STATS_VERSION,
            channels: channels
                .iter()
                .map(|snapshot| ChannelSummary {
                    server: snapshot.server.clone(),
                    channel: snapshot.channel.clone(),
                    since: snapshot.since.clone(),
                    lines_total: snapshot.lines_total,
                    lines_week: snapshot.lines_week,
                    updated_at: snapshot.updated_at,
                })
                .collect(),
        },
        60,
    )
}

pub(crate) fn channel_json(
    channels: &[PublicChannelStats],
    query: &str,
) -> anyhow::Result<RouteResponse> {
    let (Some(server), Some(channel)) =
        (query_param(query, "server"), query_param(query, "channel"))
    else {
        return Ok(not_found_json());
    };
    let Some(snapshot) = find(channels, &server, &channel) else {
        return Ok(not_found_json());
    };
    json_body(&sanitized(snapshot), 60)
}

fn sanitized(snapshot: &PublicChannelStats) -> ChannelOut<'_> {
    ChannelOut {
        version: PUBLIC_STATS_VERSION,
        server: &snapshot.server,
        channel: &snapshot.channel,
        timezone: &snapshot.timezone,
        updated_at: snapshot.updated_at,
        since: &snapshot.since,
        lines_total: snapshot.lines_total,
        lines_week: snapshot.lines_week,
        heatmap: &snapshot.heatmap,
        days: &snapshot.days,
        record_day: snapshot.record_day.as_ref(),
        boards: snapshot
            .boards
            .iter()
            .map(|board| BoardOut {
                period: &board.period,
                entries: board
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| RankOut {
                        rank: index + 1,
                        name: entry.name.as_deref(),
                        lines: entry.lines,
                    })
                    .collect(),
            })
            .collect(),
        awards: snapshot
            .awards
            .iter()
            .map(|award| AwardOut {
                title: &award.title,
                figure: &award.figure,
                name: award.name.as_deref(),
            })
            .collect(),
    }
}

fn not_found_json() -> RouteResponse {
    (
        404,
        "application/json",
        r#"{"error":"not found"}"#.into(),
        0,
    )
}

// ── HTML ────────────────────────────────────────────────────────────────────

const HEAD: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"stylesheet\" href=\"/style.css\">";
const PRIVACY: &str = "<footer>Counts only, never words. People are named only if they've made their achievements public; everyone else is “someone”. <code>!stats private</code> removes you entirely.</footer></body></html>";

/// `/stats`: the index, or one channel's page when `server` and `channel` are given.
pub(crate) fn page(channels: &[PublicChannelStats], query: &str) -> anyhow::Result<RouteResponse> {
    match (query_param(query, "server"), query_param(query, "channel")) {
        (Some(server), Some(channel)) => Ok(match find(channels, &server, &channel) {
            Some(snapshot) => (200, "text/html; charset=utf-8", channel_page(snapshot), 60),
            None => (404, "text/plain; charset=utf-8", "not found".into(), 0),
        }),
        _ => Ok((200, "text/html; charset=utf-8", index_page(channels), 60)),
    }
}

fn index_page(channels: &[PublicChannelStats]) -> String {
    let mut html = format!("{HEAD}<title>Jeeves Channel Stats</title></head><body><header><p class=\"eyebrow\">JEEVES</p><h1>Channel Stats</h1><p>Who talks, and when, in the channels that share it. <a href=\"/\">Achievement gallery →</a></p></header><main>");
    if channels.is_empty() {
        html.push_str("<p class=\"notice\">No channel has made its stats public yet.</p>");
    } else {
        html.push_str("<div class=\"grid\">");
        for snapshot in channels {
            html.push_str(&format!(
                "<a class=\"card channel-card\" href=\"/stats?{}\"><h3>{}</h3><small>{}</small><p><strong>{}</strong> lines this week · {} all time</p><small>since {}</small></a>",
                channel_query(&snapshot.server, &snapshot.channel),
                escape(&snapshot.channel),
                escape(&snapshot.server),
                grouped(snapshot.lines_week),
                grouped(snapshot.lines_total),
                escape(&snapshot.since),
            ));
        }
        html.push_str("</div>");
    }
    html.push_str("</main>");
    html.push_str(PRIVACY);
    html
}

fn channel_page(snapshot: &PublicChannelStats) -> String {
    let title = escape(&snapshot.channel);
    let mut html = format!(
        "{HEAD}<title>{title} · Jeeves Channel Stats</title></head><body><header><p class=\"eyebrow\">JEEVES · <a href=\"/stats\">CHANNEL STATS</a></p><h1>{title}</h1><p>on {} · counting since {}</p></header><main>",
        escape(&snapshot.server),
        escape(&snapshot.since),
    );
    html.push_str(&format!(
        "<section class=\"summary\"><strong>{}</strong> lines this week · <strong>{}</strong> all time",
        grouped(snapshot.lines_week),
        grouped(snapshot.lines_total)
    ));
    if let Some(record) = &snapshot.record_day {
        html.push_str(&format!(
            " · record day {} (<strong>{}</strong>)",
            escape(&record.date),
            grouped(record.lines)
        ));
    }
    html.push_str("</section>");
    html.push_str(&format!(
        "<section class=\"module stats\"><h2>When {title} talks</h2><small>{} time</small>{}</section>",
        escape(&snapshot.timezone),
        heatmap(&snapshot.heatmap)
    ));
    html.push_str(&format!(
        "<section class=\"module stats\"><h2>The last 90 days</h2>{}</section>",
        day_chart(&snapshot.days)
    ));
    html.push_str("<section class=\"module stats\"><h2>Top talkers</h2><div class=\"grid\">");
    for board in &snapshot.boards {
        let heading = match board.period.as_str() {
            "week" => "This week",
            "month" => "Last 30 days",
            _ => "All time",
        };
        html.push_str(&format!("<article class=\"card\"><h3>{heading}</h3>"));
        if board.entries.is_empty() {
            html.push_str("<p><small>Quiet so far.</small></p>");
        } else {
            html.push_str("<ol>");
            for entry in &board.entries {
                html.push_str(&format!(
                    "<li>{} <small class=\"inline\">{}</small></li>",
                    name_html(entry.name.as_deref()),
                    grouped(entry.lines)
                ));
            }
            html.push_str("</ol>");
        }
        html.push_str("</article>");
    }
    html.push_str("</div></section>");
    if !snapshot.awards.is_empty() {
        html.push_str(
            "<section class=\"module stats\"><h2>This week's awards</h2><div class=\"grid\">",
        );
        for award in &snapshot.awards {
            html.push_str(&format!(
                "<article class=\"card earned\"><h3>{}</h3><p>{}</p><small>{}</small></article>",
                escape(&award.title),
                name_html(award.name.as_deref()),
                escape(&award.figure)
            ));
        }
        html.push_str("</div></section>");
    }
    html.push_str("</main>");
    html.push_str(PRIVACY);
    html
}

fn name_html(name: Option<&str>) -> String {
    match name {
        Some(name) => escape(name),
        None => "<em>someone</em>".into(),
    }
}

/// A 7×24 table, each cell shaded by its share of the busiest hour.
fn heatmap(rows: &[Vec<u64>]) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let max = rows.iter().flatten().copied().max().unwrap_or(0).max(1);
    let mut html =
        String::from("<div class=\"scroll\"><table class=\"heatmap\"><thead><tr><th></th>");
    for hour in 0..24 {
        if hour % 3 == 0 {
            html.push_str(&format!("<th>{hour:02}</th>"));
        } else {
            html.push_str("<th></th>");
        }
    }
    html.push_str("</tr></thead><tbody>");
    for (day, name) in DAYS.iter().enumerate() {
        html.push_str(&format!("<tr><th>{name}</th>"));
        for hour in 0..24 {
            let lines = rows
                .get(day)
                .and_then(|row| row.get(hour))
                .copied()
                .unwrap_or(0);
            html.push_str(&format!(
                "<td style=\"--v:{:.2}\" title=\"{name} {hour:02}:00 · {} lines\"></td>",
                lines as f64 / max as f64,
                grouped(lines)
            ));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html
}

/// Daily totals as an inline SVG bar chart.
fn day_chart(days: &[jeeves_abi::PublicDay]) -> String {
    const WIDTH: f64 = 900.0;
    const HEIGHT: f64 = 160.0;
    let max = days.iter().map(|day| day.lines).max().unwrap_or(0).max(1) as f64;
    let step = WIDTH / days.len().max(1) as f64;
    let mut svg = format!(
        "<svg class=\"days\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" role=\"img\" aria-label=\"Lines per day\">"
    );
    for (index, day) in days.iter().enumerate() {
        let height =
            (day.lines as f64 / max * (HEIGHT - 4.0)).max(if day.lines > 0 { 2.0 } else { 0.0 });
        svg.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{height:.1}\"><title>{} · {} lines</title></rect>",
            index as f64 * step + 1.0,
            HEIGHT - height,
            (step - 2.0).max(1.0),
            escape(&day.date),
            grouped(day.lines),
        ));
    }
    svg.push_str("</svg>");
    svg
}

/// `server=…&channel=…`, percent-encoded (channels start with `#`).
fn channel_query(server: &str, channel: &str) -> String {
    let encode = |value: &str| {
        value
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>()
    };
    format!("server={}&channel={}", encode(server), encode(channel))
}

/// "1,402".
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Styles for the stats pages, appended to the gallery's.
pub(crate) const STYLE: &str = r#"
a{color:var(--accent)}.eyebrow a{color:var(--gold);text-decoration:none}.scroll{overflow-x:auto}.stats h2{text-transform:none}.heatmap{border-collapse:separate;border-spacing:3px;margin-top:.5rem;width:100%;min-width:620px;table-layout:fixed}.heatmap thead th:first-child,.heatmap tbody th{width:2.6rem}.heatmap th{color:var(--muted);font-size:.75rem;font-weight:600;padding:0 .25rem;text-align:left}.heatmap td{height:1.6rem;border-radius:5px;background:rgba(230,191,101,var(--v));border:1px solid var(--line)}.days{width:100%;height:auto;background:rgba(16,32,28,.9);border:1px solid var(--line);border-radius:14px;padding:.5rem}.days rect{fill:var(--gold)}.days rect:hover{fill:var(--accent)}.channel-card{display:block;color:var(--ink);text-decoration:none}.channel-card:hover{border-color:var(--gold)}ol{padding-left:1.4rem}li{margin:.2rem 0}small.inline{display:inline;margin-left:.4rem}.talk h2{margin:0 0 .5rem;font-family:Georgia,serif}.talk ul{margin:0;padding-left:1.2rem}code{color:var(--gold)}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::{PublicAward, PublicBoard, PublicDay, PublicRank};

    pub(crate) fn snapshot() -> PublicChannelStats {
        PublicChannelStats {
            version: PUBLIC_STATS_VERSION,
            server: "net".into(),
            channel: "#games".into(),
            timezone: "New York".into(),
            updated_at: 1,
            since: "2026-09-28".into(),
            lines_total: 1_402,
            lines_week: 40,
            heatmap: vec![vec![1; 24]; 7],
            days: vec![PublicDay {
                date: "2026-09-29".into(),
                lines: 40,
            }],
            record_day: None,
            boards: vec![PublicBoard {
                period: "all".into(),
                entries: vec![
                    PublicRank {
                        profile_id: "secret-a".into(),
                        name: None,
                        lines: 900,
                    },
                    PublicRank {
                        profile_id: "secret-b".into(),
                        name: Some("<bob>".into()),
                        lines: 502,
                    },
                ],
            }],
            awards: vec![PublicAward {
                title: "The Inquisitor".into(),
                figure: "94 questions".into(),
                profile_id: "secret-b".into(),
                name: Some("<bob>".into()),
            }],
            people: vec![PublicTalker {
                profile_id: "secret-b".into(),
                name: "<bob>".into(),
                lines: 502,
                rank: 2,
                streak: 3,
                best_streak: 9,
            }],
        }
    }

    #[test]
    fn pages_escape_names_and_keep_people_anonymous() {
        let channels = vec![snapshot()];
        let (status, _, html, _) = page(&channels, "server=net&channel=%23games").unwrap();
        assert_eq!(status, 200);
        assert!(
            html.contains("&lt;bob&gt;") && !html.contains("<bob>"),
            "{html}"
        );
        assert!(html.contains("<em>someone</em>"));
        assert!(!html.contains("secret-"), "no profile IDs in HTML");
        assert!(html.contains("1,402"));
        let (status, _, _, _) = page(&channels, "server=net&channel=%23other").unwrap();
        assert_eq!(status, 404);
        let (_, _, index, _) = page(&channels, "").unwrap();
        assert!(
            index.contains("/stats?server=net&channel=%23games"),
            "{index}"
        );
    }

    #[test]
    fn only_channels_switched_on_are_shown() {
        let db = DbHandle::open(":memory:").unwrap();
        db.kv_set_blocking(
            STATS_MODULE,
            "public:6e6574:2367616d6573",
            &serde_json::to_string(&snapshot()).unwrap(),
        )
        .unwrap();
        let settings = crate::settings::SettingRegistry::shared();
        let boolean = |key: &str| jeeves_abi::SettingSpec {
            key: key.into(),
            description: String::new(),
            default: "false".into(),
            kind: jeeves_abi::SettingKind::Boolean,
            scopes: vec![jeeves_abi::SettingScope::Channel],
            applies_immediately: true,
        };
        settings.lock().unwrap().replace_specs(vec![
            (STATS_MODULE.into(), boolean("enabled")),
            (STATS_MODULE.into(), boolean("public_page")),
        ]);
        let set = |key: &str, value: &str| {
            settings.lock().unwrap().set_override(
                STATS_MODULE,
                key,
                jeeves_abi::SettingScope::Channel,
                "net",
                "#games",
                Some(value.into()),
            );
        };
        assert!(
            public_channels(&db, &settings).unwrap().is_empty(),
            "off by default"
        );
        set("enabled", "true");
        set("public_page", "true");
        assert_eq!(public_channels(&db, &settings).unwrap().len(), 1);
        set("public_page", "false");
        assert!(
            public_channels(&db, &settings).unwrap().is_empty(),
            "switching it off hides the page at once"
        );
    }

    #[test]
    fn json_never_carries_profile_ids() {
        let channels = vec![snapshot()];
        let (status, _, body, _) = channel_json(&channels, "server=net&channel=%23games").unwrap();
        assert_eq!(status, 200);
        assert!(
            !body.contains("secret-") && !body.contains("people"),
            "{body}"
        );
        assert!(body.contains("\"name\":null"));
        let (_, _, list, _) = list_json(&channels).unwrap();
        assert!(list.contains("#games") && !list.contains("secret-"));
    }

    #[test]
    fn talk_panels_find_only_the_holder_on_their_network() {
        let mut other = snapshot();
        other.server = "elsewhere".into();
        let channels = vec![snapshot(), other];
        let lines = talk_for(&channels, "net", "secret-b");
        assert_eq!(
            lines,
            [TalkLine {
                channel: "#games".into(),
                lines: 502,
                rank: 2,
                streak: 3,
                best_streak: 9
            }]
        );
        assert!(
            talk_for(&channels, "net", "secret-a").is_empty(),
            "not public"
        );
        let html = talk_panel("net", &lines);
        assert!(html.contains("<strong>502</strong> lines in") && html.contains("(#2)"));
        assert!(html.contains("/stats?server=net&channel=%23games"));
    }
}
