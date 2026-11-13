//! The household calendar, read only: every calendar on the shared CalDAV
//! accounts, repeats expanded by the server, times in the house's zone.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};
use std::time::Duration as Wait;
use ureq::Agent;
use ureq::http::Request;

use crate::core::{Cached, Fault, Health};

/// How long a reading of the calendars is reused.
const FRESH: Wait = Wait::from_secs(300);
/// Days of calendar read ahead, today first.
pub const AHEAD: i64 = 7;

const DAV: &str = "DAV:";
const CAL: &str = "urn:ietf:params:xml:ns:caldav";

#[derive(Deserialize)]
struct Account {
    url: String,
    username: String,
    password: String,
}

pub struct Calendar {
    accounts: Vec<Account>,
    pub zone: Tz,
    agent: Agent,
    held: Cached<Vec<Event>>,
}

/// One event as the page draws it: `YYYY-MM-DD HH:MM`, or `YYYY-MM-DD`
/// for a whole day, whose end is the day after its last.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub start: String,
    pub end: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    pub calendar: String,
}

/// One iCalendar property line, unfolded.
#[derive(Debug, PartialEq)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

impl Calendar {
    /// From the accounts file, `[{"name","url","username","password"}]`,
    /// and the zone's name.
    pub fn new(accounts: &str, zone: &str) -> Result<Self, String> {
        let accounts: Vec<Account> =
            serde_json::from_str(accounts).map_err(|e| format!("caldav accounts: {e}"))?;
        let zone: Tz = zone.parse().map_err(|e| format!("HOB_TZ: {e}"))?;
        let config = Agent::config_builder()
            .timeout_global(Some(Wait::from_secs(30)))
            .http_status_as_error(false)
            // PROPFIND and REPORT are WebDAV verbs, not HTTP's own.
            .allow_non_standard_methods(true)
            // `call` follows redirects itself: ureq drops the login on a
            // redirect and may turn PROPFIND into GET.
            .max_redirects(0)
            .build();
        Ok(Calendar {
            accounts,
            zone,
            agent: Agent::new_with_config(config),
            held: Cached::new(FRESH),
        })
    }

    /// Every event from today through AHEAD days, as last read within FRESH.
    pub fn upcoming(&self) -> Result<Vec<Event>, Fault> {
        self.held.get(|| {
            let today = Utc::now().with_timezone(&self.zone).date_naive();
            self.events(today, AHEAD)
        })
    }

    pub fn health(&self) -> Health {
        self.held.tracker.health("calendar")
    }

    fn call(
        &self,
        account: &Account,
        method: &str,
        url: &str,
        depth: &str,
        body: &str,
    ) -> Result<String, String> {
        let auth = format!(
            "Basic {}",
            B64.encode(format!("{}:{}", account.username, account.password))
        );
        let mut url = url.to_owned();
        let mut hops = 0;
        let mut resp = loop {
            let req = Request::builder()
                .method(method)
                .uri(&url)
                .header("Authorization", &auth)
                .header("Depth", depth)
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body)
                .map_err(|e| format!("caldav: {e}"))?;
            let resp = self.agent.run(req).map_err(|e| format!("caldav: {e}"))?;
            let to = resp
                .headers()
                .get("Location")
                .and_then(|l| l.to_str().ok())
                .map(|l| join(&url, l));
            // Only within the same server, so the login goes nowhere else.
            match to {
                Some(to)
                    if resp.status().is_redirection()
                        && hops < 5
                        && origin(&to) == origin(&url) =>
                {
                    url = to;
                    hops += 1;
                }
                _ => break resp,
            }
        };
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("caldav: {e}"))?;
        if status >= 400 {
            return Err(format!("caldav: {status} on {method} {url}"));
        }
        Ok(text)
    }

    fn propfind(
        &self,
        account: &Account,
        url: &str,
        depth: &str,
        props: &str,
    ) -> Result<String, String> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:" xmlns:C="{CAL}"><D:prop>{props}</D:prop></D:propfind>"#
        );
        self.call(account, "PROPFIND", url, depth, &body)
    }

    /// The calendars of one account, `(name, url)`: the account's URL if
    /// it is itself a calendar, else those under its calendar home.
    fn collections(&self, account: &Account) -> Result<Vec<(String, String)>, String> {
        const PROPS: &str =
            "<D:resourcetype/><D:displayname/><D:current-user-principal/><C:calendar-home-set/>";
        // A bare server address starts at its well-known entry (RFC 6764),
        // as Fastmail's does; its root answers 404.
        let start = if account.url.trim_end_matches('/') == origin(&account.url) {
            format!("{}/.well-known/caldav", origin(&account.url))
        } else {
            account.url.clone()
        };
        let xml = self.propfind(account, &start, "0", PROPS)?;
        let doc = parse(&xml)?;
        let answers = responses(&doc);
        let (href, me) = answers.first().ok_or("caldav: empty answer")?;
        if is_calendar(*me) {
            return Ok(vec![(label(href, *me), join(&account.url, href))]);
        }
        let mut home = href_in(*me, CAL, "calendar-home-set").map(|h| join(&account.url, &h));
        if home.is_none()
            && let Some(principal) = href_in(*me, DAV, "current-user-principal")
        {
            let url = join(&account.url, &principal);
            let xml = self.propfind(account, &url, "0", "<C:calendar-home-set/>")?;
            let doc = parse(&xml)?;
            home = responses(&doc)
                .first()
                .and_then(|(_, n)| href_in(*n, CAL, "calendar-home-set"))
                .map(|h| join(&url, &h));
        }
        let home = home.ok_or("caldav: no calendar home")?;
        let xml = self.propfind(account, &home, "1", "<D:resourcetype/><D:displayname/>")?;
        let doc = parse(&xml)?;
        Ok(responses(&doc)
            .into_iter()
            .filter(|(_, n)| is_calendar(*n))
            .map(|(href, n)| (label(&href, n), join(&home, &href)))
            .collect())
    }

    /// Local midnight of a day, as CalDAV wants it: UTC, `YYYYMMDDTHHMMSSZ`.
    fn stamp(&self, day: NaiveDate) -> String {
        let midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
        let utc = self
            .zone
            .from_local_datetime(&midnight)
            .earliest()
            .map_or(midnight, |t| t.with_timezone(&Utc).naive_utc());
        utc.format("%Y%m%dT%H%M%SZ").to_string()
    }

    /// Every event touching `days` days from `first`, sorted by start.
    fn events(&self, first: NaiveDate, days: i64) -> Result<Vec<Event>, String> {
        let (from, to) = (self.stamp(first), self.stamp(first + Duration::days(days)));
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><C:calendar-query xmlns:D="DAV:" xmlns:C="{CAL}"><D:prop><C:calendar-data><C:expand start="{from}" end="{to}"/></C:calendar-data></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="{from}" end="{to}"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
        );
        let mut found = Vec::new();
        for account in &self.accounts {
            for (name, url) in self.collections(account)? {
                let xml = self.call(account, "REPORT", &url, "1", &body)?;
                let doc = parse(&xml)?;
                for (_, node) in responses(&doc) {
                    let Some(ics) = text_in(node, CAL, "calendar-data") else {
                        continue;
                    };
                    found.extend(vevents(&ics).iter().filter_map(|e| self.event(e, &name)));
                }
            }
        }
        found.sort_by(|a, b| a.start.cmp(&b.start));
        found.dedup();
        Ok(found)
    }

    fn event(&self, props: &[Prop], calendar: &str) -> Option<Event> {
        let time = |name: &str| get(props, name).and_then(|p| moment(p, self.zone));
        let text = |name: &str| {
            get(props, name)
                .map(|p| unescape(&p.value))
                .filter(|t| !t.trim().is_empty())
        };
        Some(Event {
            start: time("DTSTART")?,
            end: time("DTEND"),
            summary: text("SUMMARY").unwrap_or_else(|| "(untitled)".to_owned()),
            location: text("LOCATION"),
            calendar: calendar.to_owned(),
        })
    }
}

// --- WebDAV answers ---

fn parse(xml: &str) -> Result<Document<'_>, String> {
    Document::parse(xml).map_err(|e| format!("caldav: bad xml: {e}"))
}

/// Each `response` of a multistatus: its href and the node to read props from.
fn responses<'a, 'i>(doc: &'a Document<'i>) -> Vec<(String, Node<'a, 'i>)> {
    doc.descendants()
        .filter(|n| n.has_tag_name((DAV, "response")))
        .filter_map(|n| {
            let href = n
                .children()
                .find(|c| c.has_tag_name((DAV, "href")))?
                .text()?
                .trim()
                .to_owned();
            Some((href, n))
        })
        .collect()
}

fn find<'a, 'i>(node: Node<'a, 'i>, ns: &str, name: &str) -> Option<Node<'a, 'i>> {
    node.descendants().find(|n| n.has_tag_name((ns, name)))
}

fn text_in(node: Node, ns: &str, name: &str) -> Option<String> {
    find(node, ns, name)?.text().map(|t| t.trim().to_owned())
}

/// The href inside a property such as calendar-home-set.
fn href_in(node: Node, ns: &str, name: &str) -> Option<String> {
    text_in(find(node, ns, name)?, DAV, "href").filter(|h| !h.is_empty())
}

fn is_calendar(node: Node) -> bool {
    find(node, DAV, "resourcetype").is_some_and(|r| find(r, CAL, "calendar").is_some())
}

/// A collection's name: its display name, else the last part of its path.
fn label(href: &str, node: Node) -> String {
    text_in(node, DAV, "displayname")
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            href.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_owned()
        })
}

/// An href against its base: absolute as is, a path on the base's host.
fn join(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }
    format!("{}{href}", origin(base))
}

/// `https://host` of a URL, without path or trailing slash.
fn origin(url: &str) -> &str {
    url.find("://")
        .and_then(|i| url[i + 3..].find('/').map(|j| &url[..i + 3 + j]))
        .unwrap_or(url.trim_end_matches('/'))
}

// --- iCalendar ---

/// Lines with their continuations joined and line ends dropped.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
        } else if !raw.is_empty() {
            lines.push(raw.to_owned());
        }
    }
    lines
}

/// `NAME;PARAM=value:value`, the first colon outside quotes splitting.
fn prop(line: &str) -> Option<Prop> {
    let mut quoted = false;
    let colon = line.char_indices().find_map(|(i, c)| {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some(i),
            _ => {}
        }
        None
    })?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_ascii_uppercase(),
                v.trim_matches('"').to_owned(),
            )
        })
        .collect();
    Some(Prop {
        name,
        params,
        value: value.to_owned(),
    })
}

/// Each VEVENT's own properties, nested alarms left out.
fn vevents(ics: &str) -> Vec<Vec<Prop>> {
    let mut events = Vec::new();
    let mut current: Option<Vec<Prop>> = None;
    let mut nested = 0;
    for line in unfold(ics) {
        let upper = line.to_ascii_uppercase();
        match (upper.as_str(), &mut current) {
            ("BEGIN:VEVENT", _) => current = Some(Vec::new()),
            ("END:VEVENT", Some(_)) => events.push(current.take().unwrap_or_default()),
            (l, Some(_)) if l.starts_with("BEGIN:") => nested += 1,
            (l, Some(_)) if l.starts_with("END:") => nested -= 1,
            (_, Some(props)) if nested == 0 => props.extend(prop(&line)),
            _ => {}
        }
    }
    events
}

fn get<'a>(props: &'a [Prop], name: &str) -> Option<&'a Prop> {
    props.iter().find(|p| p.name == name)
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A stored time in the house's zone, as `YYYY-MM-DD HH:MM` or a bare
/// date: a date, a UTC time, a time in a named zone, or a floating one
/// taken as already local.
fn moment(prop: &Prop, zone: Tz) -> Option<String> {
    let v = prop.value.trim();
    if v.len() == 8 || prop.param("VALUE") == Some("DATE") {
        return NaiveDate::parse_from_str(v, "%Y%m%d")
            .ok()
            .map(|d| d.format("%Y-%m-%d").to_string());
    }
    let local = if let Some(utc) = v.strip_suffix('Z') {
        let t = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        Utc.from_utc_datetime(&t).with_timezone(&zone).naive_local()
    } else {
        let t = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
        match prop.param("TZID").and_then(|z| z.parse::<Tz>().ok()) {
            Some(tz) => tz
                .from_local_datetime(&t)
                .earliest()?
                .with_timezone(&zone)
                .naive_local(),
            None => t,
        }
    };
    Some(local.format("%Y-%m-%d %H:%M").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_events_read_in_the_houses_zone() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20261016T220000Z\r\nDTEND:20261016T230000Z\r\nSUMMARY:Dinner\\, then a film\r\nBEGIN:VALARM\r\nSUMMARY:nag\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261017\r\nDTEND;VALUE=DATE:20261018\r\nSUMMARY:Farmers\r\n  market\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let cal = Calendar::new("[]", "Europe/London").unwrap();
        let events: Vec<Event> = vevents(ics)
            .iter()
            .filter_map(|e| cal.event(e, "Home"))
            .collect();
        assert_eq!(events[0].start, "2026-10-16 23:00");
        assert_eq!(events[0].summary, "Dinner, then a film");
        assert_eq!(events[1].start, "2026-10-17");
        assert_eq!(events[1].end.as_deref(), Some("2026-10-18"));
        assert_eq!(events[1].summary, "Farmers market");
    }

    #[test]
    fn a_multistatus_yields_calendars_by_name() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/cal/home/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Family</d:displayname></d:prop></d:propstat></d:response><d:response><d:href>/cal/inbox/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response></d:multistatus>"#;
        let doc = parse(xml).unwrap();
        let cals: Vec<_> = responses(&doc)
            .into_iter()
            .filter(|(_, n)| is_calendar(*n))
            .map(|(h, n)| (label(&h, n), join("https://dav.example/cal/", &h)))
            .collect();
        assert_eq!(
            cals,
            [(
                "Family".to_owned(),
                "https://dav.example/cal/home/".to_owned()
            )]
        );
    }
}
