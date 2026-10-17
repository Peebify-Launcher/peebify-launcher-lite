// ------------ Launcher News ------------
// Turns each game's news source into one common shape (notices, news and a slideshow) for the home screen.
// Most games have their own adapter below; Hypergryph, DNA and Yostar keep theirs beside their downloaders, and the
// rest fall back to the launcher's api.json.

use serde_json::{json, Value};

use super::game_profiles::profile_str;
use super::{bluepoch, http};

pub(crate) const MAX_ITEMS: usize = 12;
pub const MAX_SLIDES: usize = 5;

pub fn int_of(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

pub fn as_i64(v: Option<&Value>) -> i64 {
    int_of(v).unwrap_or(0)
}

pub fn as_str(v: Option<&Value>) -> &str {
    v.and_then(|x| x.as_str()).unwrap_or("")
}

pub(crate) fn short_date(date: &str) -> String {
    let trimmed = date.trim();
    let first = trimmed
        .split(|c: char| c.is_whitespace() || c == 'T')
        .next()
        .unwrap_or("");
    let parts: Vec<&str> = first.split(['-', '/', '.']).collect();
    let (month, day) = match parts.as_slice() {
        [year, month, day] if year.len() == 4 => (*month, *day),
        [month, day] => (*month, *day),
        _ => return trimmed.to_string(),
    };
    match (month.parse::<u32>(), day.parse::<u32>()) {
        (Ok(m), Ok(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            format!("{m:02}-{d:02}")
        }
        _ => trimmed.to_string(),
    }
}

const UTC_PLUS_8: i32 = 8 * 3600;

pub(crate) fn local_short_date(date: &str, source_offset_secs: i32) -> String {
    use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
    let trimmed = date.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(trimmed) {
        return at.with_timezone(&Local).format("%m-%d").to_string();
    }
    let naive = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(trimmed, format).ok());
    let source = FixedOffset::east_opt(source_offset_secs);
    match (naive, source) {
        (Some(naive), Some(source)) => match source.from_local_datetime(&naive).single() {
            Some(at) => at.with_timezone(&Local).format("%m-%d").to_string(),
            None => short_date(trimmed),
        },
        _ => short_date(trimmed),
    }
}

pub fn article(content: &str, jump_url: impl AsRef<str>, time: impl AsRef<str>) -> Value {
    json!({
        "content": content,
        "jumpUrl": jump_url.as_ref(),
        "time": time.as_ref(),
    })
}

pub(crate) fn slide(url: &str, jump_url: &str) -> Value {
    json!({ "url": url, "jumpUrl": jump_url })
}

fn dedupe_take(items: Vec<&Value>, id_of: impl Fn(&Value) -> i64) -> Vec<&Value> {
    let mut seen: Vec<i64> = Vec::with_capacity(MAX_ITEMS);
    items
        .into_iter()
        .filter(|a| {
            let id = id_of(a);
            if seen.contains(&id) {
                return false;
            }
            seen.push(id);
            true
        })
        .take(MAX_ITEMS)
        .collect()
}

pub fn envelope(notice: Vec<Value>, news: Vec<Value>, slideshow: Vec<Value>) -> Value {
    json!({
        "guidance": {
            "notice": { "contents": notice },
            "news": { "contents": news },
        },
        "slideshow": slideshow,
    })
}

// ------------ Punishing: Gray Raven News ------------
// Reads Kuro's public news JSON for PGR and splits it into notices, news and banner slides.
const JSON_BASE: &str = "https://media-cdn-zspms.kurogame.net/pnswebsite/website2.0/json/G167";
const PGR_ARTICLE_URL: &str = "https://pgr.kurogame.net/news";

const TYPE_NEWS: i64 = 47;
const TYPE_NOTICE: i64 = 48;
const TYPE_EVENT: i64 = 49;

const PICTURE_TYPE_NEWS_BANNER: i64 = 8;

fn pgr_collect_articles(main: &Value, wanted: &[i64]) -> Vec<Value> {
    let empty = Vec::new();
    let mut items: Vec<&Value> = main
        .get("article")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|a| wanted.contains(&as_i64(a.get("articleType"))))
        .collect();

    items.sort_by(|a, b| as_str(b.get("createTime")).cmp(as_str(a.get("createTime"))));

    dedupe_take(items, |a| as_i64(a.get("articleId")))
        .into_iter()
        .map(|a| {
            let id = as_i64(a.get("articleId"));
            article(
                as_str(a.get("articleTitle")),
                format!("{PGR_ARTICLE_URL}/{id}"),
                local_short_date(as_str(a.get("createTime")), UTC_PLUS_8),
            )
        })
        .collect()
}

fn pgr_collect_slideshow(main: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let mut pictures: Vec<&Value> = main
        .get("picture")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|p| as_i64(p.get("pictureType")) == PICTURE_TYPE_NEWS_BANNER)
        .filter(|p| !as_str(p.get("imgUrl")).is_empty())
        .collect();

    pictures.sort_by_key(|p| std::cmp::Reverse(as_i64(p.get("sortingMark"))));

    pictures
        .into_iter()
        .take(MAX_SLIDES)
        .map(|p| slide(as_str(p.get("imgUrl")), as_str(p.get("clickUrl"))))
        .collect()
}

fn pgr_to_peebify_news(main: &Value) -> Value {
    envelope(
        pgr_collect_articles(main, &[TYPE_NOTICE]),
        pgr_collect_articles(main, &[TYPE_NEWS, TYPE_EVENT]),
        pgr_collect_slideshow(main),
    )
}

pub async fn get_pgr_news() -> Result<Value, String> {
    let main = http::get_json(&format!("{JSON_BASE}/MainMenu.json")).await?;
    Ok(pgr_to_peebify_news(&main))
}

// ------------ Girls' Frontline News ------------
// Girls' Frontline has no news site of its own, so this reads its Steam news and treats posts with words like
// maintenance or compensation in the title as notices.
const API_URL: &str = "https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/";
const GF1_ARTICLE_URL: &str = "https://store.steampowered.com/news/app";
const APP_ID: &str = "3887700";

const FETCH_COUNT: usize = 40;

const NOTICE_KEYWORDS: [&str; 6] = [
    "maintenance",
    "notice",
    "compensation",
    "downtime",
    "hotfix",
    "server",
];

fn is_notice(title: &str) -> bool {
    let title = title.to_lowercase();
    NOTICE_KEYWORDS.iter().any(|kw| title.contains(kw))
}

pub(crate) fn local_month_day(timestamp: i64) -> String {
    use chrono::TimeZone;
    let millis = if timestamp < 10_000_000_000 {
        timestamp.saturating_mul(1000)
    } else {
        timestamp
    };
    match chrono::Local.timestamp_millis_opt(millis) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.format("%m-%d").to_string()
        }
        chrono::LocalResult::None => String::new(),
    }
}

fn gf1_collect_articles(items: &[&Value], notice: bool) -> Vec<Value> {
    let mut matching: Vec<&Value> = items
        .iter()
        .copied()
        .filter(|item| is_notice(as_str(item.get("title"))) == notice)
        .collect();

    matching.sort_by_key(|item| std::cmp::Reverse(as_i64(item.get("date"))));

    dedupe_take(matching, |item| as_i64(item.get("gid")))
        .into_iter()
        .map(|item| {
            article(
                as_str(item.get("title")),
                format!(
                    "{GF1_ARTICLE_URL}/{APP_ID}/view/{}",
                    as_str(item.get("gid"))
                ),
                local_month_day(as_i64(item.get("date"))),
            )
        })
        .collect()
}

fn gf1_to_peebify_news(appnews: &Value) -> Value {
    let empty = Vec::new();
    let items: Vec<&Value> = appnews
        .get("newsitems")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|item| !as_str(item.get("title")).is_empty())
        .collect();

    envelope(
        gf1_collect_articles(&items, true),
        gf1_collect_articles(&items, false),
        Vec::new(),
    )
}

pub async fn get_gf1_news() -> Result<Value, String> {
    let url = format!("{API_URL}?appid={APP_ID}&count={FETCH_COUNT}&maxlength=1");
    let response = http::get_json(&url).await?;
    Ok(gf1_to_peebify_news(
        response.get("appnews").unwrap_or(&Value::Null),
    ))
}

// ------------ Girls' Frontline 2 News ------------
// Reads the Sunborn website API for GF2: article buckets for notices and news, plus the rotating banner slides.
const API_BASE: &str = "https://gf2-web-us-api.sunborngame.com/website";
const GF2_ARTICLE_URL: &str = "https://gf2exilium.sunborngame.com/NewsInfo";

fn gf2_collect_articles(top: &Value, buckets: &[&str]) -> Vec<Value> {
    let empty = Vec::new();
    let mut items: Vec<&Value> = buckets
        .iter()
        .flat_map(|bucket| {
            top.get(bucket)
                .and_then(|v| v.as_array())
                .unwrap_or(&empty)
                .iter()
        })
        .filter(|a| !as_str(a.get("Title")).is_empty())
        .collect();

    items.sort_by(|a, b| as_str(b.get("Date")).cmp(as_str(a.get("Date"))));

    dedupe_take(items, |a| as_i64(a.get("Id")))
        .into_iter()
        .map(|a| {
            let id = as_i64(a.get("Id"));
            let type_id = as_i64(a.get("Type"));
            article(
                as_str(a.get("Title")),
                format!("{GF2_ARTICLE_URL}?id={id}&typeId={type_id}"),
                short_date(as_str(a.get("Date"))),
            )
        })
        .collect()
}

fn gf2_banner_url(slide: &Value) -> &str {
    let launcher = as_str(slide.get("LauncherUrl"));
    if launcher.is_empty() {
        as_str(slide.get("PicUrl"))
    } else {
        launcher
    }
}

fn gf2_collect_slideshow(rotation: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let mut slides: Vec<&Value> = rotation
        .get("list")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|s| !gf2_banner_url(s).is_empty())
        .collect();

    slides.sort_by_key(|s| std::cmp::Reverse(as_i64(s.get("Sort"))));

    slides
        .into_iter()
        .take(MAX_SLIDES)
        .map(|s| slide(gf2_banner_url(s), as_str(s.get("JumpUrl"))))
        .collect()
}

fn gf2_to_peebify_news(top: &Value, rotation: &Value) -> Value {
    envelope(
        gf2_collect_articles(top, &["notice"]),
        gf2_collect_articles(top, &["newest", "news", "strategy"]),
        gf2_collect_slideshow(rotation),
    )
}

pub async fn get_gf2_news() -> Result<Value, String> {
    let top_url = format!("{API_BASE}/top_news_list");
    let rotation_url = format!("{API_BASE}/rotation");
    let (top, rotation) = tokio::join!(http::get_json(&top_url), http::get_json(&rotation_url));
    let top = top?;
    let rotation = rotation.unwrap_or_else(|e| {
        log::warn!("GF2 banner rotation failed ({e}), news will render without slides.");
        json!({})
    });
    Ok(gf2_to_peebify_news(
        top.get("data").unwrap_or(&Value::Null),
        rotation.get("data").unwrap_or(&Value::Null),
    ))
}

// ------------ Reverse: 1999 News ------------
// Reads the Bluepoch site for Reverse: 1999. Notices, events and broadcasts come from numbered collections.
const COLLECTION_NOTICE: i64 = 6;
const COLLECTION_EVENT: i64 = 7;
const COLLECTION_BROADCAST: i64 = 8;

const PAGE_SIZE: i64 = 20;
const DEFAULT_SITE: &str = "https://re1999.bluepoch.com/";

fn bp_site_url(profile: &Value) -> &str {
    profile
        .get("bpNewsUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SITE)
}

fn bp_collect(page: &[Value], site: &str, wanted: &[i64]) -> Vec<Value> {
    let mut items: Vec<&Value> = page
        .iter()
        .filter(|a| wanted.contains(&as_i64(a.get("collectionId"))))
        .filter(|a| !as_str(a.get("title")).trim().is_empty())
        .collect();

    items.sort_by(|a, b| {
        let pinned = |v: &Value| v.get("isTop").and_then(Value::as_bool).unwrap_or(false);
        pinned(b)
            .cmp(&pinned(a))
            .then_with(|| as_str(b.get("onlineTime")).cmp(as_str(a.get("onlineTime"))))
    });

    dedupe_take(items, |a| as_i64(a.get("id")))
        .into_iter()
        .map(|a| {
            article(
                as_str(a.get("title")).trim(),
                bp_article_url(a).unwrap_or(site),
                local_short_date(as_str(a.get("onlineTime")), UTC_PLUS_8),
            )
        })
        .collect()
}

fn bp_article_url(item: &Value) -> Option<&str> {
    ["jumpUrl", "linkUrl", "link"]
        .iter()
        .map(|key| as_str(item.get(*key)).trim())
        .find(|url| url.starts_with("https://") || url.starts_with("http://"))
}

fn bp_collect_slideshow(banner: &Value) -> Vec<Value> {
    let empty = Vec::new();
    banner
        .get("images")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter_map(|image| {
            let url = as_str(image.get("imageUrl"));
            (!url.is_empty()).then(|| slide(url, as_str(image.get("imageLink"))))
        })
        .take(MAX_SLIDES)
        .collect()
}

pub async fn get_bluepoch_news(profile: &Value) -> Result<Value, String> {
    let game_id = bluepoch::api_config(profile)?.game_id;
    let site = bp_site_url(profile);

    let (query, banner) = tokio::join!(
        bluepoch::activity_post(
            profile,
            "/activity/official/websites/information/query",
            json!({ "current": 1, "pageSize": PAGE_SIZE, "gameId": game_id }),
        ),
        bluepoch::activity_post(
            profile,
            "/activity/pc/launcher/banner",
            json!({ "gameId": game_id }),
        )
    );
    let query = query?;
    let banner = banner.unwrap_or_else(|e| {
        log::warn!("Bluepoch banners failed ({e}), news will render without slides.");
        Value::Null
    });

    let empty = Vec::new();
    let page = query
        .get("pageData")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    Ok(envelope(
        bp_collect(page, site, &[COLLECTION_NOTICE]),
        bp_collect(page, site, &[COLLECTION_EVENT, COLLECTION_BROADCAST]),
        bp_collect_slideshow(&banner),
    ))
}

// ------------ Brown Dust II News ------------
// Reads the Brown Dust II site feeds. Notices and maintenance go in the notice list, the rest is news.
const BD2_PAGE_SIZE: usize = 40;
const BD2_EVENT_PAGE_SIZE: usize = 12;
const BD2_NOTICE_CATEGORIES: [&str; 2] = ["notice", "inspection"];

fn bd2_article(item: &Value, site: &str, locale: &str) -> Option<Value> {
    let subject = as_str(item.get("subject")).trim();
    if subject.is_empty() {
        return None;
    }
    let jump = format!(
        "{}/{locale}/news/view?id={}",
        site.trim_end_matches('/'),
        as_str(item.get("id"))
    );
    Some(article(
        subject,
        jump,
        local_short_date(as_str(item.get("publishedAt")), 0),
    ))
}

fn bd2_collect(items: &[Value], notice: bool, site: &str, locale: &str) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::with_capacity(MAX_ITEMS);
    items
        .iter()
        .filter(|item| {
            BD2_NOTICE_CATEGORIES.contains(&as_str(item.get("category"))) == notice
        })
        .filter(|item| {
            let id = as_str(item.get("id")).to_string();
            if id.is_empty() || seen.contains(&id) {
                return false;
            }
            seen.push(id);
            true
        })
        .filter_map(|item| bd2_article(item, site, locale))
        .take(MAX_ITEMS)
        .collect()
}

fn bd2_collect_slideshow(media: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let mut items: Vec<&Value> = media.as_array().unwrap_or(&empty).iter().collect();
    items.sort_by_key(|entry| -as_i64(entry.get("priority")));
    items
        .into_iter()
        .filter_map(|entry| {
            let url = as_str(entry.get("posterUrl"));
            if url.is_empty() {
                return None;
            }
            let video = as_str(entry.get("videoId"));
            let jump = if video.is_empty() {
                String::new()
            } else {
                format!("https://www.youtube.com/watch?v={video}")
            };
            Some(slide(url, &jump))
        })
        .take(MAX_SLIDES)
        .collect()
}

pub async fn get_bd2_news(profile: &Value) -> Result<Value, String> {
    let api = profile_str(profile, "bdNewsApiUrl").trim_end_matches('/').to_string();
    let site = profile_str(profile, "bdNewsUrl").to_string();
    let locale = profile_str(profile, "bdNewsLocale").to_string();

    let page_url = format!("{api}/notices?locale={locale}&page=0&limit={BD2_PAGE_SIZE}");
    let events_url = format!(
        "{api}/notices?locale={locale}&page=0&limit={BD2_EVENT_PAGE_SIZE}&category=event"
    );
    let media_url = format!("{api}/media?locale={locale}");
    let (page, events, media) = tokio::join!(
        http::get_json(&page_url),
        http::get_json(&events_url),
        http::get_json(&media_url)
    );
    let page = page?;

    let empty = Vec::new();
    let mut items: Vec<Value> = page
        .get("items")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .clone();

    match events {
        Ok(events) => {
            if let Some(list) = events.get("items").and_then(Value::as_array) {
                items.extend(list.iter().cloned());
            }
        }
        Err(e) => log::warn!(
            "Brown Dust II events feed failed ({e}), the news bucket falls back to the general list."
        ),
    }

    items.sort_by(|a, b| as_str(b.get("publishedAt")).cmp(as_str(a.get("publishedAt"))));

    let media = media.unwrap_or_else(|e| {
        log::warn!("Brown Dust II media feed failed ({e}), news will render without slides.");
        Value::Null
    });

    Ok(envelope(
        bd2_collect(&items, true, &site, &locale),
        bd2_collect(&items, false, &site, &locale),
        bd2_collect_slideshow(&media),
    ))
}

#[cfg(test)]
mod bd2_tests {
    use super::*;

    fn page() -> Vec<Value> {
        vec![
            json!({
                "id": "01M2SD0PKYQ3MMA8MQ5DNWSJYH",
                "subject": "Collaboration PV Sharing Event — Results",
                "category": "event",
                "publishedAt": "2026-09-18T05:00:19.129Z"
            }),
            json!({
                "id": "01M2EVM39RAHH6Q42CNWF707ZE",
                "subject": "Notice of Live Update on September 16 (UTC)",
                "category": "inspection",
                "publishedAt": "2026-09-14T08:00:15.132Z"
            }),
            json!({
                "id": "01M2PGNJ0P386ZPVS1FW1AX4MC",
                "subject": "Wallpaper | Chained Soldier 2",
                "category": "notice",
                "publishedAt": "2026-09-17T07:00:26.019Z"
            }),
            json!({
                "id": "01M2PGNJ0P386ZPVS1FW1AX4MC",
                "subject": "Wallpaper | Chained Soldier 2",
                "category": "notice",
                "publishedAt": "2026-09-17T07:00:26.019Z"
            }),
            json!({
                "id": "01KZZE44B93AJK5483HAH9JCCF",
                "subject": "   ",
                "category": "shop",
                "publishedAt": "2026-09-12T01:00:00.000Z"
            }),
        ]
    }

    #[test]
    fn maintenance_rides_with_notices_and_everything_else_is_news() {
        let items = page();
        let notices = bd2_collect(&items, true, "https://www.browndust2.com", "en-us");
        let news = bd2_collect(&items, false, "https://www.browndust2.com", "en-us");

        assert_eq!(notices.len(), 2, "notice + inspection, deduped");
        assert_eq!(
            notices[0]["content"],
            "Notice of Live Update on September 16 (UTC)"
        );
        assert_eq!(notices[1]["content"], "Wallpaper | Chained Soldier 2");

        assert_eq!(news.len(), 1, "the blank-subject shop post is dropped");
        assert_eq!(news[0]["content"], "Collaboration PV Sharing Event — Results");
    }

    #[test]
    fn an_article_links_to_the_sites_own_viewer_and_shows_a_short_date() {
        let items = page();
        let news = bd2_collect(&items, false, "https://www.browndust2.com/", "en-us");
        assert_eq!(
            news[0]["jumpUrl"],
            "https://www.browndust2.com/en-us/news/view?id=01M2SD0PKYQ3MMA8MQ5DNWSJYH"
        );
        assert_eq!(
            news[0]["time"],
            local("2026-09-18T05:00:19.129Z"),
            "the ISO timestamp becomes the viewer's own date"
        );
    }

    fn local(rfc3339: &str) -> String {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d")
            .to_string()
    }

    #[test]
    fn publisher_times_are_read_in_their_own_zone() {
        assert_eq!(
            local_short_date("2026-09-26 10:00:00", UTC_PLUS_8),
            local("2026-09-26T10:00:00+08:00")
        );
        assert_eq!(
            local_short_date("2026-01-01T00:30:00", UTC_PLUS_8),
            local("2026-01-01T00:30:00+08:00")
        );
        assert_eq!(
            local_short_date("2026-09-18T05:00:19.129Z", UTC_PLUS_8),
            local("2026-09-18T05:00:19.129Z"),
            "an explicit zone wins over the publisher's"
        );
        assert_eq!(local_short_date("2026-09-18", UTC_PLUS_8), "09-18");
        assert_eq!(local_short_date("09/18", UTC_PLUS_8), "09-18");
        assert_eq!(local_short_date("", UTC_PLUS_8), "");
    }

    #[test]
    fn short_date_normalises_every_publisher_format() {
        assert_eq!(short_date("2026-09-18 10:00:00"), "09-18");
        assert_eq!(short_date("2026-09-18T05:00:19.129Z"), "09-18");
        assert_eq!(short_date("2026/9/8"), "09-08");
        assert_eq!(short_date("09/18"), "09-18");
        assert_eq!(short_date("2026.09.18"), "09-18");
        assert_eq!(short_date(""), "");
        assert_eq!(short_date("Sep 18, 2026"), "Sep 18, 2026");
        assert_eq!(short_date("2026-13-01"), "2026-13-01");
    }

    #[test]
    fn a_bluepoch_article_uses_its_own_link_when_the_feed_has_one() {
        let page = vec![
            json!({ "id": 1, "collectionId": 7, "title": "Event A", "onlineTime": "2026-09-18 10:00:00", "jumpUrl": "https://re1999.bluepoch.com/news/1" }),
            json!({ "id": 2, "collectionId": 8, "title": "Event B", "onlineTime": "2026-09-17 10:00:00" }),
        ];
        let news = bp_collect(&page, DEFAULT_SITE, &[COLLECTION_EVENT, COLLECTION_BROADCAST]);
        assert_eq!(news.len(), 2);
        assert_eq!(news[0]["jumpUrl"], "https://re1999.bluepoch.com/news/1");
        assert_eq!(news[1]["jumpUrl"], DEFAULT_SITE);
        assert_eq!(news[0]["time"], local("2026-09-18T10:00:00+08:00"));
    }

    #[test]
    fn the_envelope_carries_only_guidance_and_slides() {
        let out = envelope(Vec::new(), Vec::new(), Vec::new());
        assert!(out.get("social_media_list").is_none());
        assert!(out["guidance"]["notice"]["contents"].is_array());
        assert!(out["slideshow"].is_array());
    }

    #[test]
    fn slides_are_ordered_by_priority_and_point_at_the_video() {
        let media = json!([
            { "posterUrl": "https://x/a.png", "videoId": "aaa", "priority": 10 },
            { "posterUrl": "https://x/c.png", "videoId": "ccc", "priority": 30 },
            { "posterUrl": "", "videoId": "ddd", "priority": 40 },
            { "posterUrl": "https://x/b.png", "priority": 20 }
        ]);
        let slides = bd2_collect_slideshow(&media);
        assert_eq!(slides.len(), 3, "a poster-less entry is skipped");
        assert_eq!(slides[0]["url"], "https://x/c.png");
        assert_eq!(slides[0]["jumpUrl"], "https://www.youtube.com/watch?v=ccc");
        assert_eq!(slides[1]["url"], "https://x/b.png");
        assert_eq!(slides[1]["jumpUrl"], "", "no videoId means no link");
        assert_eq!(slides[2]["url"], "https://x/a.png");
    }

    #[test]
    fn an_empty_media_feed_is_not_an_error() {
        assert!(bd2_collect_slideshow(&Value::Null).is_empty());
        assert!(bd2_collect_slideshow(&json!([])).is_empty());
    }
}

// ------------ HoYoPlay News ------------
// The Hoyoverse launcher's web API gives each Hoyoverse game's news and banners.
mod hoyoplay {
    use serde_json::Value;

    use super::super::{http, sophon};
    use super::{article, envelope, short_date, slide, MAX_ITEMS, MAX_SLIDES};

    pub async fn get_game_content(game_id: &str) -> Result<Value, String> {
        http::get_json(&sophon::build_url("getGameContent", &[("game_id", game_id)])).await
    }

    pub fn to_peebify_news(content_response: &Value) -> Value {
        let content = &content_response["data"]["content"];
        let empty: Vec<Value> = Vec::new();
        let posts = content["posts"].as_array().unwrap_or(&empty);
        let banners = content["banners"].as_array().unwrap_or(&empty);

        let as_item = |p: &Value| {
            article(
                p.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                p.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                short_date(p.get("date").and_then(|v| v.as_str()).unwrap_or("")),
            )
        };

        let titled = |p: &&Value| !p["title"].as_str().unwrap_or("").trim().is_empty();
        let notice: Vec<Value> = posts
            .iter()
            .filter(|p| p["type"].as_str() == Some("POST_TYPE_ANNOUNCE"))
            .filter(titled)
            .take(MAX_ITEMS)
            .map(as_item)
            .collect();
        let news: Vec<Value> = posts
            .iter()
            .filter(|p| p["type"].as_str() != Some("POST_TYPE_ANNOUNCE"))
            .filter(titled)
            .take(MAX_ITEMS)
            .map(as_item)
            .collect();
        let slideshow: Vec<Value> = banners
            .iter()
            .filter_map(|b| {
                let url = b["image"]["url"].as_str().unwrap_or("");
                (!url.is_empty())
                    .then(|| slide(url, b["image"]["link"].as_str().unwrap_or("")))
            })
            .take(MAX_SLIDES)
            .collect();

        envelope(notice, news, slideshow)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn news_drops_blank_posts_and_imageless_banners_and_is_capped() {
            let mut posts: Vec<Value> = (0..20)
                .map(|i| json!({ "type": "POST_TYPE_INFO", "title": format!("news {i}"), "link": "https://j", "date": "09/18" }))
                .collect();
            posts.push(json!({ "type": "POST_TYPE_ANNOUNCE", "title": "  ", "link": "https://j", "date": "09/18" }));
            posts.push(json!({ "type": "POST_TYPE_ANNOUNCE", "title": "Maintenance", "link": "https://j", "date": "09/18" }));
            let banners: Vec<Value> = (0..8)
                .map(|i| {
                    let url = if i == 0 { String::new() } else { format!("https://img/{i}") };
                    json!({ "image": { "url": url, "link": "https://b" } })
                })
                .collect();
            let out = to_peebify_news(&json!({ "data": { "content": { "posts": posts, "banners": banners } } }));
            let notices = out["guidance"]["notice"]["contents"].as_array().unwrap();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["content"], "Maintenance");
            assert_eq!(
                out["guidance"]["news"]["contents"].as_array().unwrap().len(),
                super::MAX_ITEMS
            );
            let slides = out["slideshow"].as_array().unwrap();
            assert_eq!(slides.len(), super::MAX_SLIDES);
            assert_eq!(slides[0]["url"], "https://img/1");
            assert_eq!(slides[0]["jumpUrl"], "https://b");
        }
    }
}

// ------------ Nexon Forum News ------------
// Reads a game's Nexon community forum (Blue Archive) for the home screen. The forum page is rendered in the browser
// from a small JSON API: one call lists a board's threads, one call describes the community and carries its banner
// carousel.
mod nexon_forum {
    use serde_json::Value;

    use super::super::game_profiles::profile_str;
    use super::super::http;
    use super::{article, as_i64, as_str, envelope, local_month_day, slide, MAX_ITEMS, MAX_SLIDES};

    fn profile_boards(profile: &Value, key: &str) -> Vec<String> {
        let mut boards: Vec<String> = Vec::new();
        for entry in profile.get(key).and_then(Value::as_array).into_iter().flatten() {
            let id = match entry {
                Value::String(text) => text.trim().to_string(),
                Value::Number(number) => number.to_string(),
                _ => continue,
            };
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && !boards.contains(&id) {
                boards.push(id);
            }
        }
        boards
    }

    fn is_plain_token(text: &str) -> bool {
        !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    fn thread_url(base: &str, alias: &str, board: &str, thread: &str) -> String {
        format!("{base}/{alias}/board_view?board={board}&thread={thread}")
    }

    fn threads_url(base: &str, alias: &str, board: &str) -> String {
        format!(
            "{base}/api/v1/board/{board}/threads?alias={alias}&pageNo=1&blockStartKey=&blockStartNo=&paginationType=PAGING\
             &pageSize={MAX_ITEMS}&blockSize=5&hideType=WEB&headlineId=&searchKeywordType=THREAD_TITLE_AND_CONTENT&keywords="
        )
    }

    fn community_url(base: &str, alias: &str, country: &str) -> String {
        format!("{base}/api/v1/community/{alias}?alias={alias}&countryCode={country}")
    }

    fn error_of(body: &Value) -> Option<String> {
        let code = body.get("errorCode").and_then(Value::as_i64)?;
        let message = body.get("error").and_then(Value::as_str).unwrap_or("no message");
        Some(format!("Nexon forum answered error {code}: {message}"))
    }

    fn visible_threads(body: &Value) -> Vec<&Value> {
        body.get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|thread| {
                let hidden = thread.get("isDelete").and_then(Value::as_bool).unwrap_or(false)
                    || thread.get("isWebHide").and_then(Value::as_bool).unwrap_or(false);
                let live = thread.get("release").and_then(Value::as_str).map_or(true, |r| r == "ON");
                !hidden && live && !as_str(thread.get("title")).trim().is_empty()
            })
            .collect()
    }

    fn rows_from(body: &Value, base: &str, alias: &str, board: &str) -> Vec<(i64, i64, Value)> {
        visible_threads(body)
            .into_iter()
            .map(|thread| {
                let id = as_str(thread.get("threadId"));
                let at = as_i64(thread.get("createDate"));
                (
                    at,
                    as_i64(thread.get("threadId")),
                    article(
                        as_str(thread.get("title")).trim(),
                        thread_url(base, alias, board, id),
                        local_month_day(at),
                    ),
                )
            })
            .collect()
    }

    fn merge_rows(mut rows: Vec<(i64, i64, Value)>) -> Vec<Value> {
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        let mut seen: Vec<i64> = Vec::new();
        let mut merged = Vec::new();
        for (_, id, row) in rows {
            if id != 0 && seen.contains(&id) {
                continue;
            }
            seen.push(id);
            merged.push(row);
            if merged.len() == MAX_ITEMS {
                break;
            }
        }
        merged
    }

    fn slides_from(community: &Value, base: &str, alias: &str) -> Vec<Value> {
        let banners = community
            .pointer("/communityHome/webPlace/pcBanners")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        let home = format!("{base}/{alias}");
        let mut seen: Vec<&str> = Vec::new();
        let mut slides = Vec::new();
        for banner in banners {
            let image = as_str(banner.get("webImageUrl")).trim();
            if !image.starts_with("https://") || seen.contains(&image) {
                continue;
            }
            let target = as_str(banner.get("linkValue")).trim();
            let jump = if target.starts_with("https://") {
                target.to_string()
            } else if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
                format!("{home}/board_view?thread={target}")
            } else {
                home.clone()
            };
            seen.push(image);
            slides.push(slide(image, &jump));
            if slides.len() == MAX_SLIDES {
                break;
            }
        }
        slides
    }

    async fn get_body(url: &str) -> Result<Value, String> {
        let body = http::get_json(url).await?;
        match error_of(&body) {
            Some(error) => Err(error),
            None => Ok(body),
        }
    }

    async fn board_rows(base: &str, alias: &str, board: &str) -> Result<Vec<(i64, i64, Value)>, String> {
        let body = get_body(&threads_url(base, alias, board)).await?;
        Ok(rows_from(&body, base, alias, board))
    }

    async fn boards_rows(base: &str, alias: &str, boards: &[String]) -> (Vec<Value>, Option<String>) {
        let requests = boards.iter().map(|board| board_rows(base, alias, board));
        let mut rows = Vec::new();
        let mut loaded = false;
        let mut last_error = None;
        for result in futures::future::join_all(requests).await {
            match result {
                Ok(found) => {
                    loaded = true;
                    rows.extend(found);
                }
                Err(e) => last_error = Some(e),
            }
        }
        let error = if loaded { None } else { last_error };
        (merge_rows(rows), error)
    }

    pub async fn get_news(profile: &Value) -> Result<Value, String> {
        let base = profile_str(profile, "nexonForumUrl").trim_end_matches('/');
        let alias = profile_str(profile, "nexonForumAlias");
        let country = match profile_str(profile, "nexonCountryCode") {
            "" => "US",
            country => country,
        };
        if !base.starts_with("https://") || !is_plain_token(alias) || !is_plain_token(country) {
            return Err("profile is missing a valid Nexon forum address".to_string());
        }
        let notice_boards = profile_boards(profile, "nexonNoticeBoards");
        let news_boards = profile_boards(profile, "nexonNewsBoards");

        let community_address = community_url(base, alias, country);
        let ((notices, notice_error), (news, news_error), community) = futures::join!(
            boards_rows(base, alias, &notice_boards),
            boards_rows(base, alias, &news_boards),
            get_body(&community_address),
        );

        let slides = match &community {
            Ok(body) => slides_from(body, base, alias),
            Err(e) => {
                log::warn!("news: Nexon forum banners failed ({e}); showing none");
                Vec::new()
            }
        };
        if notices.is_empty() && news.is_empty() && slides.is_empty() {
            return Err(notice_error
                .or(news_error)
                .or(community.err())
                .unwrap_or_else(|| "the Nexon forum returned no posts".to_string()));
        }
        Ok(envelope(notices, news, slides))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        const BASE: &str = "https://forum.nexon.com";
        const ALIAS: &str = "bluearchive-en";

        #[test]
        fn threads_become_articles_that_open_on_the_forum() {
            let body = json!({ "threads": [
                { "threadId": "3554410", "title": " Toy Pre-Orders ", "createDate": 1790668800, "release": "ON", "isSticky": true },
                { "threadId": "3550470", "title": "Developer's Letter", "createDate": 1790132400, "release": "ON" },
                { "threadId": "1", "title": "Deleted", "createDate": 1, "isDelete": true },
                { "threadId": "2", "title": "Hidden on web", "createDate": 2, "isWebHide": true },
                { "threadId": "3", "title": "Draft", "createDate": 3, "release": "OFF" },
                { "threadId": "4", "title": "   ", "createDate": 4 },
            ]});
            let rows = rows_from(&body, BASE, ALIAS, "3028");
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].2["content"], "Toy Pre-Orders");
            assert_eq!(
                rows[0].2["jumpUrl"],
                "https://forum.nexon.com/bluearchive-en/board_view?board=3028&thread=3554410"
            );
        }

        #[test]
        fn boards_merge_newest_first_without_repeats_and_are_capped() {
            let mut rows = vec![
                (100, 1, article("Old", "https://x/1", "")),
                (300, 3, article("New", "https://x/3", "")),
                (200, 2, article("Mid", "https://x/2", "")),
                (300, 3, article("New again", "https://x/3", "")),
            ];
            let merged = merge_rows(rows.clone());
            let titles: Vec<&str> = merged.iter().filter_map(|a| a["content"].as_str()).collect();
            assert_eq!(titles, vec!["New", "Mid", "Old"]);
            for n in 10..40 {
                rows.push((n, n, article("Filler", "https://x/f", "")));
            }
            assert_eq!(merge_rows(rows).len(), MAX_ITEMS);
        }

        #[test]
        fn forum_banners_become_slides_with_safe_links() {
            let community = json!({ "communityHome": { "webPlace": { "pcBanners": [
                { "webImageUrl": "https://cdn.example/a.png", "linkType": "WEB_LINK", "linkValue": "https://forum.nexon.com/bluearchive-en/board_view?board=3218&thread=1" },
                { "webImageUrl": "https://cdn.example/a.png", "linkValue": "https://forum.nexon.com/other" },
                { "webImageUrl": "https://cdn.example/b.png", "linkValue": "3550498" },
                { "webImageUrl": "https://cdn.example/c.png", "linkValue": "javascript:alert(1)" },
                { "webImageUrl": "http://cdn.example/insecure.png", "linkValue": "3" },
                { "webImageUrl": "", "linkValue": "4" },
            ]}}});
            let slides = slides_from(&community, BASE, ALIAS);
            assert_eq!(slides.len(), 3);
            assert_eq!(slides[0]["jumpUrl"], "https://forum.nexon.com/bluearchive-en/board_view?board=3218&thread=1");
            assert_eq!(slides[1]["jumpUrl"], "https://forum.nexon.com/bluearchive-en/board_view?thread=3550498");
            assert_eq!(slides[2]["jumpUrl"], "https://forum.nexon.com/bluearchive-en");
        }

        #[test]
        fn board_ids_must_be_plain_numbers() {
            let profile = json!({ "nexonNoticeBoards": ["3028", 3219, "3028", "../x", "", true] });
            assert_eq!(profile_boards(&profile, "nexonNoticeBoards"), vec!["3028".to_string(), "3219".to_string()]);
            assert!(profile_boards(&json!({}), "nexonNoticeBoards").is_empty());
        }

        #[test]
        fn an_error_body_is_an_error_not_empty_news() {
            let failed = json!({ "errorCode": 4, "error": "invalid parameter" });
            assert!(error_of(&failed).unwrap().contains("invalid parameter"));
            assert!(error_of(&json!({ "threads": [] })).is_none());
        }
    }
}

pub(super) use channels::get_news_data;

// ------------ News Command and Cache ------------
// The get-news-data command. It picks the right adapter for a game, keeps results for ten minutes, and falls back
// to the last good copy if a fetch fails. The last good copy is also kept on disk, so a fresh start can show it at
// once (`{ saved: true }`) while the real fetch runs.
mod channels {
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use serde_json::{json, Map, Value};
    use tauri::{AppHandle, Manager};

    use super::super::BackendState;
    use super::super::{
        arg_str, err_response, game_profiles, http, hypergryph, ok_with, resolve_profile, yostar,
    };
    use super::{hoyoplay, nexon_forum};

    const NEWS_CACHE_TTL: Duration = Duration::from_secs(60 * 10);

    static NEWS_CACHE: http::TtlCache<Value> =
        http::TtlCache::new(NEWS_CACHE_TTL, game_profiles::GAME_IDS.len());

    const SAVED_NEWS_NAME: &str = "news-cache.json";

    static SAVED: Mutex<Option<Map<String, Value>>> = Mutex::new(None);

    fn saved_path(app: &AppHandle) -> std::path::PathBuf {
        app.state::<BackendState>().user_data.join(SAVED_NEWS_NAME)
    }

    async fn saved_news(app: &AppHandle, game_id: &str) -> Option<Value> {
        if SAVED.lock().is_none() {
            let path = saved_path(app);
            let read = tauri::async_runtime::spawn_blocking(move || {
                std::fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            SAVED.lock().get_or_insert(read);
        }
        SAVED.lock().as_ref()?.get(game_id).cloned()
    }

    fn save_news(app: &AppHandle, game_id: &str, news: &Value) {
        let bytes = {
            let mut saved = SAVED.lock();
            let map = saved.get_or_insert_with(Map::new);
            if map.get(game_id) == Some(news) {
                return;
            }
            map.insert(game_id.to_string(), news.clone());
            serde_json::to_vec(map)
        };
        let Ok(bytes) = bytes else {
            return;
        };
        let path = saved_path(app);
        tauri::async_runtime::spawn_blocking(move || {
            if let Err(e) = super::super::fs_util::write_atomic(&path, &bytes) {
                log::debug!("news: could not save the copy on disk: {e}");
            }
        });
    }

    fn news_response(game_id: &str, news: Value) -> Value {
        ok_with(json!({ "gameId": game_id, "data": news }))
    }

    fn news_counts(news: &Value) -> String {
        let count = |pointer: &str| {
            news.pointer(pointer)
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        format!(
            "notices={} news={} slides={}",
            count("/guidance/notice/contents"),
            count("/guidance/news/contents"),
            count("/slideshow")
        )
    }

    fn serve_fetched(
        game_id: &str,
        source: &str,
        started: Instant,
        fetched: Result<Value, String>,
        stale: Option<Value>,
    ) -> Value {
        match fetched {
            Ok(news) => {
                log::info!(
                    "news: {game_id} via {source} ok {} in {} ms",
                    news_counts(&news),
                    started.elapsed().as_millis()
                );
                NEWS_CACHE.set(game_id, news.clone());
                news_response(game_id, news)
            }
            Err(e) => stale_or_fail(game_id, source, stale, &e),
        }
    }

    fn stale_or_fail(game_id: &str, source: &str, stale: Option<Value>, error: &str) -> Value {
        match stale {
            Some(data) => {
                log::warn!("news: {game_id} via {source} failed ({error}); served stale");
                news_response(game_id, data)
            }
            None => {
                log::error!("news: {game_id} via {source} failed ({error}); nothing cached to serve");
                err_response("Failed to load news data")
            }
        }
    }

    async fn fetch_hoyoplay_news(profile: &Value) -> Result<Value, String> {
        let Some(hoyo_game_id) = profile.get("hoyoGameId").and_then(Value::as_str) else {
            return Err("profile has no hoyoGameId".to_string());
        };
        let content = hoyoplay::get_game_content(hoyo_game_id).await?;
        Ok(hoyoplay::to_peebify_news(&content))
    }

    async fn fetch_adapter_news(profile: &Value) -> Option<(&'static str, Result<Value, String>)> {
        let install_mode = game_profiles::install_mode(profile);
        let news_source = profile.get("newsSource").and_then(|v| v.as_str());
        let pair = match (install_mode, news_source) {
            (Some("sophon"), _) => ("HoYoPlay", fetch_hoyoplay_news(profile).await),
            (Some("hypergryph"), _) => ("Hypergryph", hypergryph::get_news(profile).await),
            (Some("gf2"), _) => ("GF2", super::get_gf2_news().await),
            (Some("bluepoch"), _) => ("Bluepoch", super::get_bluepoch_news(profile).await),
            (Some("bd2"), _) => ("Brown Dust II", super::get_bd2_news(profile).await),
            (Some("dna"), _) => ("Duet Night Abyss", super::super::dna::get_news(profile).await),
            (Some("yostar"), _) => ("Yostar", yostar::get_news(profile).await),
            (_, Some("nexon")) => ("Nexon forum", nexon_forum::get_news(profile).await),
            (_, Some("pgr")) => ("PGR", super::get_pgr_news().await),
            (_, Some("gf1")) => ("GF1", super::get_gf1_news().await),
            (Some("netease"), _) => ("NTE", super::nte_site::get_nte_news().await),
            _ => return None,
        };
        Some(pair)
    }

    fn serve_and_save(
        app: &AppHandle,
        game_id: &str,
        source: &str,
        started: Instant,
        fetched: Result<Value, String>,
        stale: Option<Value>,
    ) -> Value {
        if let Ok(news) = &fetched {
            save_news(app, game_id, news);
        }
        serve_fetched(game_id, source, started, fetched, stale)
    }

    pub(crate) async fn get_news_data(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
        let profile = resolve_profile(app, arg_str(args, 0));
        let game_id = game_profiles::profile_id(profile);

        let cached = NEWS_CACHE.get_aged(game_id);
        if let Some((data, true)) = &cached {
            return Ok(news_response(game_id, data.clone()));
        }
        let stale = match cached.map(|(data, _)| data) {
            Some(data) => Some(data),
            None => saved_news(app, game_id).await,
        };

        let saved_only = args.get(1).and_then(|o| o.get("saved")).and_then(Value::as_bool) == Some(true);
        if saved_only {
            return Ok(ok_with(json!({ "gameId": game_id, "data": stale })));
        }
        let started = Instant::now();

        if let Some((source, fetched)) = fetch_adapter_news(profile).await {
            return Ok(serve_and_save(app, game_id, source, started, fetched, stale));
        }

        if !super::super::api_config::peebify_content_on(app) {
            return Ok(news_response(
                game_id,
                stale.unwrap_or_else(|| super::envelope(Vec::new(), Vec::new(), Vec::new())),
            ));
        }

        let api_config = app.state::<BackendState>().api_config.clone();
        if !api_config.is_loaded() && http::is_online().await {
            if let Err(e) = api_config.initialize().await {
                log::warn!("news: API config still empty after lazy init attempt: {e}");
            }
        }

        let fetched = match api_config.news_url_for_client(Some(game_id)) {
            Some(url) => http::get_json(&url).await,
            None => Err("the API config is not loaded".to_string()),
        };
        Ok(serve_and_save(app, game_id, "api.json", started, fetched, stale))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn news_counts_reads_the_envelope() {
            let payload = super::super::envelope(
                vec![json!({}), json!({})],
                vec![json!({})],
                Vec::new(),
            );
            assert_eq!(news_counts(&payload), "notices=2 news=1 slides=0");
            assert_eq!(news_counts(&json!({})), "notices=0 news=0 slides=0");
        }

        #[test]
        fn failure_serves_stale_without_touching_the_cache() {
            let key = "news-test-stale";
            let stale = json!({ "slideshow": [1] });
            let served = serve_fetched(
                key,
                "test",
                Instant::now(),
                Err("offline".to_string()),
                Some(stale.clone()),
            );
            assert_eq!(served["success"], json!(true));
            assert_eq!(served["gameId"], json!(key));
            assert_eq!(served["data"], stale);
            assert!(NEWS_CACHE.get_aged(key).is_none());
        }

        #[test]
        fn failure_without_stale_is_an_error_and_is_not_cached() {
            let key = "news-test-empty";
            let served = serve_fetched(key, "test", Instant::now(), Err("offline".to_string()), None);
            assert_eq!(served["success"], json!(false));
            assert!(NEWS_CACHE.get_aged(key).is_none());
        }

        #[test]
        fn success_is_cached_under_the_requested_game() {
            let key = "news-test-ok";
            let payload = super::super::envelope(Vec::new(), Vec::new(), Vec::new());
            let served = serve_fetched(key, "test", Instant::now(), Ok(payload.clone()), None);
            assert_eq!(served["gameId"], json!(key));
            assert_eq!(NEWS_CACHE.get(key), Some(payload));
        }
    }
}

// ------------ Neverness to Everness News ------------
// Neverness to Everness posts news on a plain HTML site, so this scrapes the list pages and article banners and
// builds the slideshow from the newest articles.
mod nte_site {
    use std::sync::OnceLock;
    use std::time::Duration;

    use regex::Regex;
    use serde_json::{json, Value};

    use super::super::http;

    const NTE_ORIGIN: &str = "https://nte.perfectworld.com";
    const SLIDESHOW_ARTICLE_COUNT: usize = 5;
    const LIST_PAGES_PER_SECTION: usize = 3;
    const BANNER_CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 12);

    static BANNER_CACHE: http::TtlCache<Option<String>> = http::TtlCache::new(BANNER_CACHE_TTL, 64);

    const NEWS_SECTION: &str = "gamenews";
    const NOTICES_SECTION: &str = "gamebroad";

    fn list_page_url(section: &str, page: usize) -> String {
        let file = if page == 0 {
            "index.html".to_string()
        } else {
            format!("index{page}.html")
        };
        format!("{NTE_ORIGIN}/en/article/news/{section}/{file}")
    }

    fn entity_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(r"&(#[xX][0-9a-fA-F]+|#[0-9]+|[a-zA-Z][a-zA-Z0-9]*);").expect("static regex")
        })
    }

    fn tag_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"<[^>]*>").expect("static regex"))
    }

    fn list_item_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(
                r#"<a\s[^>]*?href="([^"]+)"[^>]*>\s*<div class="listItem">[\s\S]*?<h2 class="title">([\s\S]*?)</h2>[\s\S]*?<p class="date">([\s\S]*?)</p>"#,
            )
            .expect("static regex")
        })
    }

    fn article_content_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(
                r#"<div class="articleContent">([\s\S]*?)</div>\s*<div class="rel articleBottom""#,
            )
            .expect("static regex")
        })
    }

    fn img_src_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r#"<img[^>]+src="([^"]+)""#).expect("static regex"))
    }

    fn decode_html_entities(text: &str) -> String {
        entity_re()
            .replace_all(text, |caps: &regex::Captures<'_>| {
                let body = &caps[1];
                if let Some(numeric) = body.strip_prefix('#') {
                    let code = if let Some(hex) = numeric
                        .strip_prefix('x')
                        .or_else(|| numeric.strip_prefix('X'))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        numeric.parse::<u32>().ok()
                    };
                    return match code.and_then(char::from_u32) {
                        Some(c) => c.to_string(),
                        None => caps[0].to_string(),
                    };
                }
                match body {
                    "amp" => "&".to_string(),
                    "lt" => "<".to_string(),
                    "gt" => ">".to_string(),
                    "quot" => "\"".to_string(),
                    "apos" => "'".to_string(),
                    "nbsp" => " ".to_string(),
                    "lsquo" => "\u{2018}".to_string(),
                    "rsquo" => "\u{2019}".to_string(),
                    "ldquo" => "\u{201C}".to_string(),
                    "rdquo" => "\u{201D}".to_string(),
                    "ndash" => "\u{2013}".to_string(),
                    "mdash" => "\u{2014}".to_string(),
                    "hellip" => "\u{2026}".to_string(),
                    _ => caps[0].to_string(),
                }
            })
            .into_owned()
    }

    fn strip_tags(html: &str) -> String {
        tag_re().replace_all(html, "").into_owned()
    }

    fn clean_text(raw: &str) -> String {
        decode_html_entities(&strip_tags(raw)).trim().to_string()
    }

    struct ListItem {
        url: String,
        title: String,
        date: String,
    }

    fn parse_news_list_page(html: &str, page_url: &str) -> Vec<ListItem> {
        let base = url::Url::parse(page_url).ok();
        list_item_re()
            .captures_iter(html)
            .filter_map(|caps| {
                let href = &caps[1];
                let url = base.as_ref()?.join(href).ok()?.to_string();
                Some(ListItem {
                    url,
                    title: clean_text(&caps[2]),
                    date: clean_text(&caps[3]),
                })
            })
            .collect()
    }

    fn parse_article_banner_image(html: &str, article_url: &str) -> Option<String> {
        let content = article_content_re().captures(html)?;
        let img = img_src_re().captures(content.get(1)?.as_str())?;
        url::Url::parse(article_url)
            .ok()?
            .join(&img[1])
            .ok()
            .map(|u| u.to_string())
    }

    async fn fetch_news_list(url: String) -> Result<Vec<ListItem>, String> {
        let html = http::get_text(&url)
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        Ok(parse_news_list_page(&html, &url))
    }

    async fn fetch_news_section(section: &str) -> (Vec<ListItem>, Vec<String>, usize) {
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        let mut failures = Vec::new();
        let mut requests = 0;
        for page in 0..LIST_PAGES_PER_SECTION {
            if items.len() >= super::MAX_ITEMS {
                break;
            }
            requests += 1;
            match fetch_news_list(list_page_url(section, page)).await {
                Ok(page) if page.is_empty() => break,
                Ok(page) => items.extend(page.into_iter().filter(|item| seen.insert(item.url.clone()))),
                Err(e) => failures.push(e),
            }
        }
        (items, failures, requests)
    }

    fn check_list_failures(failures: &[String], requests: usize) -> Result<(), String> {
        let Some(first) = failures.first() else {
            return Ok(());
        };
        if failures.len() >= requests {
            return Err(format!(
                "all {requests} NTE list pages failed, first error: {first}"
            ));
        }
        log::warn!(
            "NTE news: {} of {requests} list pages failed, first error: {first}",
            failures.len()
        );
        Ok(())
    }

    async fn fetch_article_banner(item_url: String) -> Option<Value> {
        if let Some(cached) = BANNER_CACHE.get(&item_url) {
            let image = cached?;
            return Some(json!({ "url": image, "jumpUrl": item_url }));
        }
        let image = match http::get_text(&item_url).await {
            Ok(html) => parse_article_banner_image(&html, &item_url),
            Err(e) => {
                log::warn!("NTE news: failed to fetch article {item_url}: {e}");
                return None;
            }
        };
        BANNER_CACHE.set(&item_url, image.clone());
        Some(json!({ "url": image?, "jumpUrl": item_url }))
    }

    pub async fn get_nte_news() -> Result<Value, String> {
        let (
            (news_items, news_failures, news_requests),
            (notice_items, notice_failures, notice_requests),
        ) = tokio::join!(
            fetch_news_section(NEWS_SECTION),
            fetch_news_section(NOTICES_SECTION),
        );
        let failures: Vec<String> = news_failures.into_iter().chain(notice_failures).collect();
        check_list_failures(&failures, news_requests + notice_requests)?;
        if news_items.is_empty() && notice_items.is_empty() {
            return Err("NTE news: no articles parsed from any list page".to_string());
        }

        let banner_futures = news_items
            .iter()
            .take(SLIDESHOW_ARTICLE_COUNT)
            .map(|item| fetch_article_banner(item.url.clone()));
        let banner_slides: Vec<Value> = futures::future::join_all(banner_futures)
            .await
            .into_iter()
            .flatten()
            .collect();

        let as_guidance_item = |item: &ListItem| {
            super::article(&item.title, &item.url, super::short_date(&item.date))
        };

        Ok(super::envelope(
            notice_items
                .iter()
                .take(super::MAX_ITEMS)
                .map(as_guidance_item)
                .collect(),
            news_items
                .iter()
                .take(super::MAX_ITEMS)
                .map(as_guidance_item)
                .collect(),
            banner_slides,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn list_failures_error_only_when_every_page_failed() {
            assert!(check_list_failures(&[], 6).is_ok());
            let some = ["a".to_string(), "b".to_string()];
            assert!(check_list_failures(&some, 6).is_ok());
            let all: Vec<String> = (0..6).map(|i| i.to_string()).collect();
            let err = check_list_failures(&all, 6).unwrap_err();
            assert!(err.contains("all 6 NTE list pages failed"));
        }

        #[test]
        fn titles_decode_named_decimal_and_hex_entities() {
            assert_eq!(
                decode_html_entities("It&rsquo;s &ldquo;here&rdquo; &mdash; 1&ndash;2&hellip;"),
                "It\u{2019}s \u{201C}here\u{201D} \u{2014} 1\u{2013}2\u{2026}"
            );
            assert_eq!(decode_html_entities("&#39;&#x27;&#X27;"), "'''");
            assert_eq!(decode_html_entities("Tom &amp; Jerry &lt;3"), "Tom & Jerry <3");
            assert_eq!(decode_html_entities("&#12a; &bogus;"), "&#12a; &bogus;");
        }
    }
}
