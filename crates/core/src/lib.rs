// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Download engine. No HTTP server, no UI.

mod backend;
mod category;
mod certificate;
mod download;
mod error;
mod filename;
mod ftp;
mod http;
mod id;
mod job;
mod links;
mod manager;
mod playback;
mod problem;
mod rename;
mod segmented;
mod sequence;
mod settings;
mod slot;
mod speed;
mod store;
mod stream;
mod throttle;
mod transfer;
mod view;
mod webmark;

pub use backend::{Backend, HttpBackend, NativeBackend};
pub use category::{BuiltIn, Category, CategoryId, CategorySettings, Matched};
pub use certificate::{CertificateId, TrustedCertificate};
pub use download::{
    Download, Event, Outcome, Placement, RequestHeaders, RestartReason, Segment, Validator,
};
pub use error::Error;
pub use filename::{Existing, PART_SUFFIX};
pub use http::{HttpConfig, build_client, run};
pub use id::JobId;
pub use job::{JobChange, JobEvent, JobSnapshot, JobState, NewJob, Priority, Reorder};
pub use links::LinkPreview;
pub use manager::Manager;
pub use playback::Playback;
pub use problem::Problem;
pub use settings::{Appearance, AppearanceMode, Palette, RetryPolicy, Settings, SettingsChange};
pub use slot::{Downloads, Repeat, Slot, SlotId, SlotSettings, time_zone_name};
pub use store::Store;
pub use stream::{Opened, Reader};
pub use throttle::Throttle;
pub use view::{AddedWithin, SortDirection, SortKey, View, ViewId, ViewSettings};

/// The URL type every download uses, so clients don't need `reqwest` themselves.
pub use reqwest::Url;

/// The calendar types time slots use, so clients don't need `jiff` themselves.
pub use jiff::civil::{Date, Time, Weekday};
