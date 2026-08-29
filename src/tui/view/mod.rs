use std::collections::BTreeSet;
use std::env;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
    Wrap,
};
use zeroize::Zeroize;

use crate::controller::{
    ConnectionInfo, ConnectionsSnapshot, NodeReachabilityAssessment, ProbeOutcome,
};
use crate::private_access::{PrivateAccessAuthField, PrivateAccessState};
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::storage::NodeLatencySample;
use crate::subscriptions::SubscriptionRefreshOutput;

mod connections;
mod dashboard;
mod help;
mod latency_chart;
mod onboarding;
mod private_access;
mod settings;
mod shared;
mod status;

pub(super) use connections::*;
pub(super) use dashboard::*;
pub(super) use help::*;
pub(super) use latency_chart::*;
pub(super) use onboarding::*;
pub(super) use private_access::*;
pub(super) use settings::*;
pub(super) use shared::*;
pub(super) use status::*;
