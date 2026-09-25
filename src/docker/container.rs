//! Runtime-neutral view of one row of a container listing.
//!
//! Every backend (the Docker API today) converts its own listing type into
//! [`ContainerInfo`], so the scheduler, `check`, the probe and the
//! network-dependent scan never see a daemon-specific type. The fields mirror
//! what those callers read from bollard's `ContainerSummary`, keeping its
//! optionality: a missing `Image` or `Id` is still an error where it was one.

use std::collections::HashMap;

use bollard::models::{ContainerSummary, ContainerSummaryStateEnum};

/// The listing's coarse container state. Only the states callers distinguish
/// get their own variant; everything else (created, exited, dead, removing,
/// or no state reported) is `Other`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListedState {
    Running,
    Restarting,
    Paused,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerInfo {
    pub id: Option<String>,
    /// As the daemon lists them; Docker prefixes each with `/`.
    pub names: Option<Vec<String>>,
    /// The image reference the container was created from, or a bare image id
    /// when the daemon no longer has a tag for it.
    pub image: Option<String>,
    pub image_id: Option<String>,
    pub labels: Option<HashMap<String, String>>,
    /// `HostConfig.NetworkMode`: `bridge`, a network name, `container:<ref>`, …
    pub network_mode: Option<String>,
    pub state: ListedState,
}

impl From<ContainerSummary> for ContainerInfo {
    fn from(c: ContainerSummary) -> Self {
        Self {
            id: c.id,
            names: c.names,
            image: c.image,
            image_id: c.image_id,
            labels: c.labels,
            network_mode: c.host_config.and_then(|hc| hc.network_mode),
            state: match c.state {
                Some(ContainerSummaryStateEnum::RUNNING) => ListedState::Running,
                Some(ContainerSummaryStateEnum::RESTARTING) => ListedState::Restarting,
                Some(ContainerSummaryStateEnum::PAUSED) => ListedState::Paused,
                _ => ListedState::Other,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::ContainerSummaryHostConfig;

    use super::*;

    #[test]
    fn a_summary_converts_field_for_field() {
        let summary = ContainerSummary {
            id: Some("abc".to_owned()),
            names: Some(vec!["/web".to_owned()]),
            image: Some("nginx:1.27".to_owned()),
            image_id: Some("sha256:img".to_owned()),
            labels: Some(HashMap::from([("k".to_owned(), "v".to_owned())])),
            host_config: Some(ContainerSummaryHostConfig {
                network_mode: Some("container:vpn".to_owned()),
                ..Default::default()
            }),
            state: Some(ContainerSummaryStateEnum::RESTARTING),
            ..Default::default()
        };
        assert_eq!(
            ContainerInfo::from(summary),
            ContainerInfo {
                id: Some("abc".to_owned()),
                names: Some(vec!["/web".to_owned()]),
                image: Some("nginx:1.27".to_owned()),
                image_id: Some("sha256:img".to_owned()),
                labels: Some(HashMap::from([("k".to_owned(), "v".to_owned())])),
                network_mode: Some("container:vpn".to_owned()),
                state: ListedState::Restarting,
            }
        );
    }

    #[test]
    fn states_callers_do_not_distinguish_collapse_to_other() {
        for state in [
            None,
            Some(ContainerSummaryStateEnum::EXITED),
            Some(ContainerSummaryStateEnum::CREATED),
            Some(ContainerSummaryStateEnum::DEAD),
        ] {
            let summary = ContainerSummary {
                state,
                ..Default::default()
            };
            assert_eq!(ContainerInfo::from(summary).state, ListedState::Other);
        }
    }
}
