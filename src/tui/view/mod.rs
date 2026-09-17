use std::collections::BTreeSet;
use std::env;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use zeroize::Zeroize;

use crate::controller::{
    ConnectionInfo, ConnectionsSnapshot, NodeReachabilityAssessment, ProbeOutcome,
};
use crate::private_access::{PrivateAccessAuthField, PrivateAccessState};
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::subscriptions::SubscriptionRefreshOutput;
use crate::sustained_quality::{NodeSustainedQuality, SustainedProbeOutcome};
use crate::usability_probe::ManifestDiagnostic;

mod command_palette;
mod connections;
mod dashboard;
mod help;
mod node_dashboard;
mod node_quality_detail;
mod onboarding;
mod private_access;
mod provider_modal;
mod settings;
mod shared;
mod status;

pub(super) use command_palette::*;
pub(super) use connections::*;
pub(super) use dashboard::*;
pub(super) use help::*;
pub(super) use node_dashboard::*;
pub(super) use node_quality_detail::*;
pub(super) use onboarding::*;
pub(super) use private_access::*;
pub(super) use provider_modal::*;
pub(super) use settings::*;
pub(super) use shared::*;
pub(super) use status::*;
