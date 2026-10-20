//! What applies to the whole manager, and how it changes.

use std::time::Duration;

use crate::filename::Existing;

/// Settings that apply to the whole manager. Stored as the single row of the
/// `settings` table; the defaults here match the table's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Bytes per second for all downloads together; 0 = unlimited.
    pub global_limit: u64,
    /// How many jobs may download at once. At least 1.
    pub max_concurrent: usize,
    pub retry: RetryPolicy,
    /// The most links added at once (docs/decisions/0036). At least 1.
    pub max_batch: usize,
    /// New video and music downloads start in streaming mode, unless the
    /// one adding them says otherwise (docs/decisions/0029).
    pub stream_media: bool,
    /// The user has been through the first-start setup (or skipped it), so
    /// front ends shouldn't offer it again.
    pub setup_done: bool,
    /// The last step of that setup someone went past (done or skipped),
    /// while it isn't done, so it carries on after it; `None`: from the
    /// start. The steps are the front ends' own; the engine only keeps it
    /// (docs/decisions/0050).
    pub setup_step: Option<String>,
    /// The look shared by the devices that follow it; `None` until one
    /// shares its own. The engine only keeps it (docs/decisions/0022).
    pub appearance: Option<Appearance>,
    /// The language those devices show, as a tag like `en` or `pt-BR`;
    /// `None`: each shows its own system's (docs/decisions/0041).
    pub language: Option<String>,
    /// What a download does when a file with its name is already in its
    /// folder, unless the one adding it chose (docs/decisions/0049).
    pub existing: Existing,
    /// While saving to the database fails, the last error's text (not saved,
    /// and no change sets it: the manager follows the store's writes).
    pub storage_failing: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            global_limit: 0,
            max_concurrent: 3,
            retry: RetryPolicy::default(),
            max_batch: 10_000,
            stream_media: true,
            setup_done: false,
            setup_step: None,
            appearance: None,
            language: None,
            existing: Existing::Rename,
            storage_failing: None,
        }
    }
}

impl Settings {
    /// Applies `change`. Values aren't checked against the ranges people may
    /// choose (the API does that); a `max_concurrent` or `max_batch` of 0
    /// counts as 1.
    pub(crate) fn apply(&mut self, change: SettingsChange) {
        let SettingsChange {
            global_limit,
            max_concurrent,
            max_retries,
            retries_before_yield,
            first_retry_wait,
            max_batch,
            stream_media,
            setup_done,
            setup_step,
            appearance,
            language,
            existing,
        } = change;
        if let Some(limit) = global_limit {
            self.global_limit = limit;
        }
        if let Some(max) = max_concurrent {
            self.max_concurrent = max.max(1);
        }
        if let Some(max) = max_retries {
            self.retry.max_retries = max;
        }
        if let Some(retries) = retries_before_yield {
            self.retry.retries_before_yield = retries;
        }
        if let Some(wait) = first_retry_wait {
            self.retry.first_wait = wait;
        }
        if let Some(max) = max_batch {
            self.max_batch = max.max(1);
        }
        if let Some(on) = stream_media {
            self.stream_media = on;
        }
        if let Some(done) = setup_done {
            self.setup_done = done;
        }
        if let Some(step) = setup_step {
            self.setup_step = step;
        }
        if let Some(appearance) = appearance {
            self.appearance = Some(appearance);
        }
        if let Some(language) = language {
            self.language = language;
        }
        if let Some(existing) = existing {
            self.existing = existing;
        }
    }
}

/// Some settings to change at once; `None` leaves one as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsChange {
    pub global_limit: Option<u64>,
    pub max_concurrent: Option<usize>,
    pub max_retries: Option<u32>,
    pub retries_before_yield: Option<u32>,
    pub first_retry_wait: Option<Duration>,
    pub max_batch: Option<usize>,
    pub stream_media: Option<bool>,
    pub setup_done: Option<bool>,
    /// `Some(None)`: the setup starts from its first step.
    pub setup_step: Option<Option<String>>,
    pub appearance: Option<Appearance>,
    /// `Some(None)`: back to each device's system language.
    pub language: Option<Option<String>>,
    pub existing: Option<Existing>,
}

/// What happens when a download fails with an error that may go away (a
/// dropped connection, a busy server). See docs/decisions/0013.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the attempts since bytes last arrived; then the job
    /// fails. 0 = never retry.
    pub max_retries: u32,
    /// Retries a job makes in its download slot before giving way to a job
    /// that is waiting for one.
    pub retries_before_yield: u32,
    /// Wait before the first retry. Each next one waits twice as long, up to
    /// a minute (or this wait, if longer); a server's `Retry-After` can ask for more.
    pub first_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 10,
            retries_before_yield: 3,
            first_wait: Duration::from_secs(2),
        }
    }
}

/// How the front ends look: light, dark or as the system says, and which
/// pair of themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    pub mode: AppearanceMode,
    pub palette: Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceMode {
    System,
    Light,
    Dark,
}

/// A light theme and a dark one that go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    /// Seafoam and Lagoon.
    Sea,
    /// Pearl and Slate.
    Neutral,
}

impl AppearanceMode {
    /// As stored in the database.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AppearanceMode::System => "system",
            AppearanceMode::Light => "light",
            AppearanceMode::Dark => "dark",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Self::System, Self::Light, Self::Dark]
            .into_iter()
            .find(|mode| mode.as_str() == text)
    }
}

impl Palette {
    /// As stored in the database.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Palette::Sea => "sea",
            Palette::Neutral => "neutral",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Self::Sea, Self::Neutral]
            .into_iter()
            .find(|palette| palette.as_str() == text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_sets_only_its_fields() {
        let mut settings = Settings::default();
        settings.apply(SettingsChange {
            max_retries: Some(4),
            appearance: Some(Appearance {
                mode: AppearanceMode::Dark,
                palette: Palette::Neutral,
            }),
            ..SettingsChange::default()
        });
        let expected = Settings {
            retry: RetryPolicy {
                max_retries: 4,
                ..RetryPolicy::default()
            },
            appearance: Some(Appearance {
                mode: AppearanceMode::Dark,
                palette: Palette::Neutral,
            }),
            ..Settings::default()
        };
        assert_eq!(settings, expected);

        settings.apply(SettingsChange::default());
        assert_eq!(settings, expected);
    }

    #[test]
    fn at_least_one_download_at_once() {
        let mut settings = Settings::default();
        settings.apply(SettingsChange {
            max_concurrent: Some(0),
            ..SettingsChange::default()
        });
        assert_eq!(settings.max_concurrent, 1);
    }

    #[test]
    fn appearance_names_round_trip() {
        for mode in [
            AppearanceMode::System,
            AppearanceMode::Light,
            AppearanceMode::Dark,
        ] {
            assert_eq!(AppearanceMode::parse(mode.as_str()), Some(mode));
        }
        for palette in [Palette::Sea, Palette::Neutral] {
            assert_eq!(Palette::parse(palette.as_str()), Some(palette));
        }
        assert_eq!(Palette::parse("teal"), None);
    }
}
