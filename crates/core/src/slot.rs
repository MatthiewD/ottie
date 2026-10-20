//! Time slots (docs/decisions/0040): when downloads may run, are stopped or
//! limited, for the whole queue or one category, by the daemon's own clock.
//! The manager keeps them and applies what the active ones say.

use std::collections::HashMap;
use std::fmt;

use jiff::civil::{Date, Time, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::category::CategoryId;
use crate::error::Error;

/// Identifies a slot. Like [`CategoryId`], a plain number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u32);

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Which days a slot happens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repeat {
    /// Every week on these days (at least one).
    Weekly(Vec<Weekday>),
    /// Once, on this date; the slot is removed once it's over.
    Once(Date),
}

/// What a slot does to downloads while it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downloads {
    /// They may run (over a lower slot that stops them).
    Allow,
    /// None start. Running ones stop, unless `let_finish`: then they finish,
    /// and only new ones wait. When it ends, those it held carry on, unless
    /// `stay_paused`: then they're paused, to be resumed by hand.
    Stop { let_finish: bool, stay_paused: bool },
}

/// What the user sets on a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotSettings {
    /// May be empty.
    pub name: String,
    /// Off: kept, but it never applies.
    pub enabled: bool,
    /// The category it applies to; `None`: the whole queue.
    pub category: Option<CategoryId>,
    pub repeat: Repeat,
    /// From this time of day (to the minute)...
    pub from: Time,
    /// ...to this one, on the next day if it isn't later (the same time: a
    /// whole day).
    pub to: Time,
    /// `None`: leaves downloads as they are.
    pub downloads: Option<Downloads>,
    /// Bytes per second for the queue (or the category) while it lasts; 0 =
    /// no limit. `None`: the usual limit.
    pub speed_limit: Option<u64>,
    /// How many may run at once while it lasts (at least 1). `None`: the usual.
    pub max_concurrent: Option<usize>,
}

/// A slot as listed and announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub id: SlotId,
    /// Its place in the list: lower comes first, and wins. Only the order
    /// means something.
    pub place: u64,
    pub settings: SlotSettings,
    /// It applies right now.
    pub active: bool,
}

impl SlotSettings {
    /// Every day from `from` to `to`, changing nothing yet.
    pub fn daily(from: Time, to: Time) -> Self {
        Self {
            name: String::new(),
            enabled: true,
            category: None,
            repeat: Repeat::Weekly(ALL_DAYS.to_vec()),
            from,
            to,
            downloads: None,
            speed_limit: None,
            max_concurrent: None,
        }
    }

    /// Trims the name, drops seconds, puts the days in order without
    /// repeats; refuses a weekly slot without days, or one that does nothing.
    pub(crate) fn normalized(mut self) -> Result<Self, Error> {
        self.name = self.name.trim().to_owned();
        if let Repeat::Weekly(days) = &mut self.repeat {
            *days = ALL_DAYS
                .into_iter()
                .filter(|day| days.contains(day))
                .collect();
            if days.is_empty() {
                return Err(Error::SlotWithoutDays);
            }
        }
        if self.downloads.is_none() && self.speed_limit.is_none() && self.max_concurrent.is_none() {
            return Err(Error::SlotWithoutEffect);
        }
        self.from = to_minute(self.from);
        self.to = to_minute(self.to);
        self.max_concurrent = self.max_concurrent.map(|max| max.max(1));
        Ok(self)
    }

    /// Whether it applies at `now` (on/off aside).
    pub(crate) fn covers(&self, now: &Zoned) -> bool {
        let at = now.timestamp();
        self.spans(yesterday(now.date()), 2, now.time_zone())
            .into_iter()
            .any(|(start, end)| start <= at && at < end)
    }

    /// The next moment after `now` it starts or ends, within the coming week.
    pub(crate) fn next_change(&self, now: &Zoned) -> Option<Timestamp> {
        let at = now.timestamp();
        self.spans(yesterday(now.date()), 9, now.time_zone())
            .into_iter()
            .flat_map(|(start, end)| [start, end])
            .filter(|&change| change > at)
            .min()
    }

    /// It stops downloads, and those it held stay paused once it ends.
    pub(crate) fn keeps_paused(&self) -> bool {
        matches!(
            self.downloads,
            Some(Downloads::Stop {
                stay_paused: true,
                ..
            })
        )
    }

    /// A slot that happens once, and that's over.
    pub(crate) fn is_over(&self, now: &Zoned) -> bool {
        let Repeat::Once(date) = self.repeat else {
            return false;
        };
        self.span_on(date, now.time_zone())
            .is_none_or(|(_, end)| end <= now.timestamp())
    }

    /// Each time it happens, as [start, end), on the `count` days from `first`.
    fn spans(&self, first: Date, count: usize, tz: &TimeZone) -> Vec<(Timestamp, Timestamp)> {
        let mut spans = Vec::new();
        let mut date = first;
        for _ in 0..count {
            let on = match &self.repeat {
                Repeat::Weekly(days) => days.contains(&date.weekday()),
                Repeat::Once(once) => *once == date,
            };
            if on && let Some(span) = self.span_on(date, tz) {
                spans.push(span);
            }
            let Ok(next) = date.tomorrow() else {
                break;
            };
            date = next;
        }
        spans
    }

    /// When it starts and ends if it starts on `date`. A time the clocks skip
    /// that day counts as the same time after the change (02:30 is 03:30 when
    /// they go forward at 02:00); one they repeat counts the first time.
    fn span_on(&self, date: Date, tz: &TimeZone) -> Option<(Timestamp, Timestamp)> {
        let end_date = if self.to <= self.from {
            date.tomorrow().ok()?
        } else {
            date
        };
        let at = |date: Date, time: Time| {
            tz.to_ambiguous_zoned(date.to_datetime(time))
                .compatible()
                .ok()
                .map(|zoned| zoned.timestamp())
        };
        Some((at(date, self.from)?, at(end_date, self.to)?))
    }
}

/// Monday first, as the days of a weekly slot are kept.
pub(crate) const ALL_DAYS: [Weekday; 7] = [
    Weekday::Monday,
    Weekday::Tuesday,
    Weekday::Wednesday,
    Weekday::Thursday,
    Weekday::Friday,
    Weekday::Saturday,
    Weekday::Sunday,
];

fn to_minute(time: Time) -> Time {
    time.with()
        .second(0)
        .subsec_nanosecond(0)
        .build()
        .unwrap_or(time)
}

fn yesterday(date: Date) -> Date {
    date.yesterday().unwrap_or(date)
}

/// What the active slots change right now: for each effect, the highest
/// slot in the list that sets it, for the whole queue and each category.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Effects {
    pub queue: Effect,
    pub categories: HashMap<CategoryId, Effect>,
}

/// What one target (the queue, or a category) is told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Effect {
    pub downloads: Option<(SlotId, Downloads)>,
    pub speed_limit: Option<u64>,
    pub max_concurrent: Option<usize>,
}

impl Effects {
    /// `active`: the slots that apply now, highest in the list first.
    pub(crate) fn of<'a>(active: impl IntoIterator<Item = (SlotId, &'a SlotSettings)>) -> Self {
        let mut effects = Effects::default();
        for (id, slot) in active {
            let effect = match slot.category {
                None => &mut effects.queue,
                Some(category) => effects.categories.entry(category).or_default(),
            };
            effect.downloads = effect
                .downloads
                .or(slot.downloads.map(|downloads| (id, downloads)));
            effect.speed_limit = effect.speed_limit.or(slot.speed_limit);
            effect.max_concurrent = effect.max_concurrent.or(slot.max_concurrent);
        }
        effects
    }

    /// The slot stopping downloads of `category`, if one does, and whether
    /// it lets running ones finish. A stopped queue stops every category;
    /// when the queue and the category both stop, running downloads finish
    /// only if both let them: the one that stops them at once wins, else the
    /// queue's.
    pub(crate) fn stop(&self, category: CategoryId) -> Option<(SlotId, bool)> {
        let of_category = self.categories.get(&category).and_then(|e| e.downloads);
        [self.queue.downloads, of_category]
            .into_iter()
            .flatten()
            .filter_map(|(id, downloads)| match downloads {
                Downloads::Stop { let_finish, .. } => Some((id, let_finish)),
                Downloads::Allow => None,
            })
            .reduce(|queue, category| {
                if queue.1 && !category.1 {
                    category
                } else {
                    queue
                }
            })
    }

    pub(crate) fn of_category(&self, category: CategoryId) -> Effect {
        self.categories.get(&category).copied().unwrap_or_default()
    }
}

/// The name of the time zone the slots follow (the system's), like
/// `Europe/Paris`, if it has one.
pub fn time_zone_name() -> Option<String> {
    TimeZone::system().iana_name().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::{date, time};

    fn paris() -> TimeZone {
        TimeZone::get("Europe/Paris").unwrap()
    }

    /// 2026-10-14 is a Wednesday.
    fn at(day: i8, hour: i8, minute: i8) -> Zoned {
        date(2026, 10, day)
            .at(hour, minute, 0, 0)
            .to_zoned(paris())
            .unwrap()
    }

    fn slot(repeat: Repeat, from: Time, to: Time) -> SlotSettings {
        SlotSettings {
            repeat,
            downloads: Some(Downloads::Stop {
                let_finish: false,
                stay_paused: false,
            }),
            ..SlotSettings::daily(from, to)
        }
    }

    #[test]
    fn a_slot_covers_its_hours_on_its_days() {
        let work = slot(
            Repeat::Weekly(vec![Weekday::Wednesday]),
            time(9, 0, 0, 0),
            time(18, 0, 0, 0),
        );
        assert!(!work.covers(&at(14, 8, 59)));
        assert!(work.covers(&at(14, 9, 0)));
        assert!(work.covers(&at(14, 17, 59)));
        assert!(!work.covers(&at(14, 18, 0)), "the end isn't in it");
        assert!(!work.covers(&at(15, 10, 0)), "Thursday");
    }

    #[test]
    fn a_night_slot_runs_into_the_next_day() {
        let night = slot(
            Repeat::Weekly(vec![Weekday::Wednesday]),
            time(23, 0, 0, 0),
            time(7, 0, 0, 0),
        );
        assert!(night.covers(&at(14, 23, 30)));
        assert!(
            night.covers(&at(15, 6, 59)),
            "Thursday morning, from Wednesday"
        );
        assert!(!night.covers(&at(15, 23, 30)), "but not Thursday night");
        assert_eq!(
            night.next_change(&at(14, 12, 0)),
            Some(at(14, 23, 0).timestamp())
        );
        assert_eq!(
            night.next_change(&at(14, 23, 0)),
            Some(at(15, 7, 0).timestamp())
        );
        // A week later.
        assert_eq!(
            night.next_change(&at(15, 7, 0)),
            Some(at(21, 23, 0).timestamp())
        );
    }

    #[test]
    fn the_same_time_twice_is_a_whole_day() {
        let saturday = slot(
            Repeat::Weekly(vec![Weekday::Saturday]),
            time(0, 0, 0, 0),
            time(0, 0, 0, 0),
        );
        assert!(saturday.covers(&at(17, 0, 0)));
        assert!(saturday.covers(&at(17, 23, 59)));
        assert!(!saturday.covers(&at(18, 0, 0)));
    }

    #[test]
    fn a_slot_once_is_over_after_its_end() {
        let tonight = slot(
            Repeat::Once(date(2026, 10, 14)),
            time(23, 0, 0, 0),
            time(1, 0, 0, 0),
        );
        assert!(tonight.covers(&at(15, 0, 30)));
        assert!(!tonight.covers(&at(21, 23, 30)), "not a week later");
        assert!(!tonight.is_over(&at(15, 0, 59)));
        assert!(tonight.is_over(&at(15, 1, 0)));
        let weekly = slot(
            Repeat::Weekly(ALL_DAYS.to_vec()),
            time(1, 0, 0, 0),
            time(2, 0, 0, 0),
        );
        assert!(!weekly.is_over(&at(15, 3, 0)));
    }

    #[test]
    fn slots_follow_the_wall_clock_when_it_changes() {
        // Paris goes to winter time on 2026-10-25: 03:00 becomes 02:00.
        let early = slot(
            Repeat::Weekly(ALL_DAYS.to_vec()),
            time(1, 0, 0, 0),
            time(4, 0, 0, 0),
        );
        let changed = |hours| {
            date(2026, 10, 25)
                .at(1, 0, 0, 0)
                .to_zoned(paris())
                .unwrap()
                .timestamp()
                + jiff::SignedDuration::from_hours(hours)
        };
        let sunday = Zoned::new(changed(0), paris());
        let (start, end) = early.span_on(sunday.date(), &paris()).unwrap();
        assert_eq!((start, end), (changed(0), changed(4)), "an hour longer");
        // Summer time on 2027-03-28: 02:00 becomes 03:00; 02:30 is skipped.
        let skipped = slot(
            Repeat::Weekly(ALL_DAYS.to_vec()),
            time(2, 30, 0, 0),
            time(5, 0, 0, 0),
        );
        let (start, _) = skipped.span_on(date(2027, 3, 28), &paris()).unwrap();
        let after = date(2027, 3, 28).at(3, 30, 0, 0).to_zoned(paris()).unwrap();
        assert_eq!(start, after.timestamp());
    }

    #[test]
    fn normalizing_tidies_and_refuses_what_makes_no_sense() {
        let messy = SlotSettings {
            name: "  Night ".into(),
            repeat: Repeat::Weekly(vec![Weekday::Sunday, Weekday::Monday, Weekday::Sunday]),
            from: time(23, 0, 42, 5),
            max_concurrent: Some(0),
            ..SlotSettings::daily(time(23, 0, 0, 0), time(7, 0, 0, 0))
        }
        .normalized()
        .unwrap();
        assert_eq!(messy.name, "Night");
        assert_eq!(
            messy.repeat,
            Repeat::Weekly(vec![Weekday::Monday, Weekday::Sunday])
        );
        assert_eq!(messy.from, time(23, 0, 0, 0));
        assert_eq!(messy.max_concurrent, Some(1));

        let no_days = SlotSettings {
            repeat: Repeat::Weekly(Vec::new()),
            speed_limit: Some(0),
            ..SlotSettings::daily(time(1, 0, 0, 0), time(2, 0, 0, 0))
        };
        assert!(matches!(no_days.normalized(), Err(Error::SlotWithoutDays)));
        let idle = SlotSettings::daily(time(1, 0, 0, 0), time(2, 0, 0, 0));
        assert!(matches!(idle.normalized(), Err(Error::SlotWithoutEffect)));
    }

    #[test]
    fn the_highest_slot_wins_each_effect() {
        let (night, work, videos) = (SlotId(1), SlotId(2), SlotId(3));
        let stop = SlotSettings {
            downloads: Some(Downloads::Stop {
                let_finish: true,
                stay_paused: false,
            }),
            ..SlotSettings::daily(time(1, 0, 0, 0), time(2, 0, 0, 0))
        };
        let limit = SlotSettings {
            downloads: Some(Downloads::Allow),
            speed_limit: Some(1000),
            ..stop.clone()
        };
        let quiet = SlotSettings {
            category: Some(CategoryId(4)),
            max_concurrent: Some(1),
            ..stop.clone()
        };
        // `work` is higher: its allow and limit win; `night` adds nothing.
        let effects = Effects::of([(work, &limit), (night, &stop), (videos, &quiet)]);
        assert_eq!(effects.queue.downloads, Some((work, Downloads::Allow)));
        assert_eq!(effects.queue.speed_limit, Some(1000));
        assert_eq!(effects.stop(CategoryId::GENERAL), None);
        // A category stopped on its own stays stopped.
        assert_eq!(effects.stop(CategoryId(4)), Some((videos, true)));
        assert_eq!(effects.of_category(CategoryId(4)).max_concurrent, Some(1));

        let effects = Effects::of([(night, &stop), (work, &limit)]);
        assert_eq!(effects.stop(CategoryId(4)), Some((night, true)));
        assert_eq!(effects.queue.speed_limit, Some(1000), "set by work alone");
    }

    #[test]
    fn stopping_at_once_wins_over_letting_finish() {
        let (night, videos) = (SlotId(1), SlotId(2));
        let stop = |let_finish, category| SlotSettings {
            category,
            downloads: Some(Downloads::Stop {
                let_finish,
                stay_paused: false,
            }),
            ..SlotSettings::daily(time(1, 0, 0, 0), time(2, 0, 0, 0))
        };
        let (gentle_queue, strict_queue) = (stop(true, None), stop(false, None));
        let gentle_videos = stop(true, Some(CategoryId(4)));
        let strict_videos = stop(false, Some(CategoryId(4)));

        let effects = Effects::of([(night, &gentle_queue), (videos, &strict_videos)]);
        assert_eq!(effects.stop(CategoryId(4)), Some((videos, false)));
        assert_eq!(effects.stop(CategoryId::GENERAL), Some((night, true)));
        let effects = Effects::of([(night, &strict_queue), (videos, &gentle_videos)]);
        assert_eq!(effects.stop(CategoryId(4)), Some((night, false)));
        // Both let them finish: the queue's, as before.
        let effects = Effects::of([(night, &gentle_queue), (videos, &gentle_videos)]);
        assert_eq!(effects.stop(CategoryId(4)), Some((night, true)));
    }
}
