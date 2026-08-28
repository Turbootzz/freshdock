//! Docker Compose project awareness (issue #78).
//!
//! Compose writes its whole dependency graph into container labels, so a
//! project is reconstructible from the socket alone: no compose file to locate,
//! no `docker compose` binary. This module is pure (labels in, graph out); the
//! daemon listing lives in [`crate::docker`] and the rollout in
//! [`crate::rollout`].
//!
//! `com.docker.compose.depends_on` is a comma-joined list of
//! `service:condition:restart` triples. The third field is compose's
//! `depends_on.<service>.restart`, NOT `required`, verified against compose
//! 5.3.1. Leaf services carry the label with an empty value, and the entries
//! come out of a Go map, so their order is not stable.

use std::collections::{HashMap, HashSet};

use tracing::warn;

pub const LABEL_PROJECT: &str = "com.docker.compose.project";
pub const LABEL_SERVICE: &str = "com.docker.compose.service";
pub const LABEL_DEPENDS_ON: &str = "com.docker.compose.depends_on";
pub const LABEL_ONEOFF: &str = "com.docker.compose.oneoff";

/// The condition a dependency has to reach before its dependent may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Condition {
    /// `service_started`: proceed as soon as the dependency is up.
    Started,
    /// `service_healthy`: wait for the dependency's healthcheck to pass.
    Healthy,
    /// `service_completed_successfully`: a one-shot that must exit zero first.
    CompletedSuccessfully,
}

impl Condition {
    /// An unknown token degrades to the weakest condition rather than failing,
    /// so a future compose release cannot stall a rollout.
    fn parse(raw: &str) -> Self {
        match raw.trim() {
            "service_healthy" => Condition::Healthy,
            "service_completed_successfully" => Condition::CompletedSuccessfully,
            _ => Condition::Started,
        }
    }
}

/// One edge of the project graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub service: String,
    pub condition: Condition,
    /// Compose's `depends_on.<service>.restart`.
    pub restart: bool,
}

/// A container's place in its compose project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeInfo {
    pub project: String,
    pub service: String,
    /// Direct dependencies only, sorted by service name.
    pub depends_on: Vec<Dependency>,
}

/// One container of a compose project, as the rollout planner needs it. Not
/// bollard's `ContainerSummary`, so the planner stays free of daemon types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMember {
    pub name: String,
    pub id: String,
    /// The container's `Image` as the daemon reports it. Note that a listing
    /// falls back to a bare image id once the tag it was created from has moved.
    pub image_ref: String,
    /// The resolved image id, which still matches when `image_ref` does not.
    pub image_id: Option<String>,
    pub labels: HashMap<String, String>,
    pub running: bool,
}

/// A container's compose identity, or `None` when it is not a project member
/// freshdock should roll out. `oneoff=True` is a `docker compose run` leftover:
/// it carries the project's labels but is not part of the declared stack.
pub fn parse(labels: &HashMap<String, String>) -> Option<ComposeInfo> {
    if labels
        .get(LABEL_ONEOFF)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    {
        return None;
    }
    let project = non_empty(labels.get(LABEL_PROJECT))?;
    let service = non_empty(labels.get(LABEL_SERVICE))?;
    Some(ComposeInfo {
        project,
        service,
        depends_on: parse_depends_on(labels.get(LABEL_DEPENDS_ON).map_or("", String::as_str)),
    })
}

fn non_empty(value: Option<&String>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Parse the `com.docker.compose.depends_on` value. Tolerant of missing fields
/// (the label has changed shape before), and sorted so a plan does not depend
/// on Go's map iteration order.
pub fn parse_depends_on(raw: &str) -> Vec<Dependency> {
    let mut deps: Vec<Dependency> = raw
        .split(',')
        .filter_map(|entry| {
            let mut fields = entry.split(':');
            let service = fields.next()?.trim();
            if service.is_empty() {
                return None;
            }
            let condition = fields.next().map_or(Condition::Started, Condition::parse);
            let restart = fields
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
            Some(Dependency {
                service: service.to_owned(),
                condition,
                restart,
            })
        })
        .collect();
    deps.sort_by(|a, b| a.service.cmp(&b.service));
    deps.dedup_by(|a, b| a.service == b.service);
    deps
}

/// The project's dependency graph. Built from every member, not just the
/// rollout's targets: ordering a subset needs the edges that leave it.
pub fn graph_of(members: &[ProjectMember]) -> HashMap<String, Vec<Dependency>> {
    members
        .iter()
        .filter_map(|m| parse(&m.labels))
        .map(|info| (info.service, info.depends_on))
        .collect()
}

/// Services some other service waits on with `service_completed_successfully`:
/// the project's one-shots, whose *correct* state is `exited 0`. The only
/// unlabelled containers a rollout may touch.
pub fn services_awaited_for_completion(
    graph: &HashMap<String, Vec<Dependency>>,
) -> HashSet<String> {
    graph
        .values()
        .flatten()
        .filter(|d| d.condition == Condition::CompletedSuccessfully)
        .map(|d| d.service.clone())
        .collect()
}

/// Order `nodes` so every service comes after its declared dependencies.
///
/// Kahn over the subgraph induced by `nodes`; edges leaving the set impose no
/// ordering on it. Ties break on name so a plan is reproducible. A cycle (only
/// reachable via a hand-written label, compose rejects them) drains in name
/// order rather than hanging.
pub fn topological_order(
    nodes: &[String],
    graph: &HashMap<String, Vec<Dependency>>,
) -> Vec<String> {
    let set: HashSet<&str> = nodes.iter().map(String::as_str).collect();
    let mut pending: HashMap<&str, HashSet<&str>> = HashMap::new();
    for node in nodes.iter().map(String::as_str) {
        let deps: HashSet<&str> = graph
            .get(node)
            .map(|deps| {
                deps.iter()
                    .map(|d| d.service.as_str())
                    .filter(|s| set.contains(s) && *s != node)
                    .collect()
            })
            .unwrap_or_default();
        pending.insert(node, deps);
    }

    let mut ordered = Vec::with_capacity(nodes.len());
    while !pending.is_empty() {
        let mut ready: Vec<&str> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(node, _)| *node)
            .collect();
        if ready.is_empty() {
            let mut stuck: Vec<&str> = pending.keys().copied().collect();
            stuck.sort_unstable();
            warn!(
                services = %stuck.join(", "),
                "compose: depends_on has a cycle; rolling the remaining services out in name order"
            );
            ordered.extend(stuck.iter().map(|s| (*s).to_owned()));
            break;
        }
        ready.sort_unstable();
        for node in &ready {
            pending.remove(node);
        }
        for deps in pending.values_mut() {
            deps.retain(|d| !ready.contains(d));
        }
        ordered.extend(ready.iter().map(|s| (*s).to_owned()));
    }
    ordered
}

/// Services that declare `depends_on.<dep>.restart = true` on one of `updated`,
/// i.e. asked in the compose file to be restarted when it is recreated.
pub fn restart_dependents(
    graph: &HashMap<String, Vec<Dependency>>,
    updated: &HashSet<String>,
) -> Vec<String> {
    let mut services: Vec<String> = graph
        .iter()
        .filter(|(service, _)| !updated.contains(*service))
        .filter(|(_, deps)| {
            deps.iter()
                .any(|d| d.restart && updated.contains(&d.service))
        })
        .map(|(service, _)| service.clone())
        .collect();
    services.sort();
    services
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn member(name: &str, service: &str, deps: &str) -> ProjectMember {
        ProjectMember {
            name: name.to_owned(),
            id: format!("id-{name}"),
            image_ref: "app:latest".to_owned(),
            image_id: Some("sha256:app".to_owned()),
            labels: labels(&[
                (LABEL_PROJECT, "stack"),
                (LABEL_SERVICE, service),
                (LABEL_DEPENDS_ON, deps),
            ]),
            running: true,
        }
    }

    /// The exact label compose 5.3.1 writes for
    /// `depends_on: {migrate: {condition: service_completed_successfully},
    ///  db: {condition: service_healthy, restart: true}}`.
    #[test]
    fn parses_the_label_compose_actually_writes() {
        let info = parse(&labels(&[
            (LABEL_PROJECT, "fdlab"),
            (LABEL_SERVICE, "web"),
            (LABEL_ONEOFF, "False"),
            (
                LABEL_DEPENDS_ON,
                "migrate:service_completed_successfully:false,db:service_healthy:true",
            ),
        ]))
        .expect("compose member");

        assert_eq!(info.project, "fdlab");
        assert_eq!(info.service, "web");
        // Sorted by service name, not the order compose emitted them in.
        assert_eq!(
            info.depends_on,
            vec![
                Dependency {
                    service: "db".to_owned(),
                    condition: Condition::Healthy,
                    restart: true,
                },
                Dependency {
                    service: "migrate".to_owned(),
                    condition: Condition::CompletedSuccessfully,
                    restart: false,
                },
            ]
        );
    }

    #[test]
    fn a_leaf_service_has_an_empty_depends_on_label() {
        let info = parse(&labels(&[
            (LABEL_PROJECT, "fdlab"),
            (LABEL_SERVICE, "db"),
            (LABEL_DEPENDS_ON, ""),
        ]))
        .expect("compose member");
        assert!(info.depends_on.is_empty());
    }

    #[test]
    fn a_container_without_compose_labels_is_not_a_member() {
        assert!(parse(&labels(&[("freshdock.enable", "true")])).is_none());
        assert!(
            parse(&labels(&[(LABEL_PROJECT, "fdlab")])).is_none(),
            "no service"
        );
        assert!(
            parse(&labels(&[(LABEL_SERVICE, "web")])).is_none(),
            "no project"
        );
        assert!(
            parse(&labels(&[(LABEL_PROJECT, "  "), (LABEL_SERVICE, "web")])).is_none(),
            "a blank project label is not a project"
        );
    }

    #[test]
    fn a_compose_run_oneoff_is_never_a_rollout_member() {
        let base = [
            (LABEL_PROJECT, "fdlab"),
            (LABEL_SERVICE, "web"),
            (LABEL_ONEOFF, "True"),
        ];
        assert!(parse(&labels(&base)).is_none());
        // Compose writes `True`/`False`; be case-insensitive anyway.
        assert!(parse(&labels(&[base[0], base[1], (LABEL_ONEOFF, "true")])).is_none());
        assert!(parse(&labels(&[base[0], base[1], (LABEL_ONEOFF, "False")])).is_some());
    }

    #[test]
    fn depends_on_parsing_tolerates_short_and_ragged_entries() {
        // Condition and restart both missing (an older compose shape).
        assert_eq!(
            parse_depends_on("db"),
            vec![Dependency {
                service: "db".to_owned(),
                condition: Condition::Started,
                restart: false,
            }]
        );
        // Restart missing.
        assert_eq!(
            parse_depends_on("db:service_healthy")[0].condition,
            Condition::Healthy
        );
        // Blank entries and stray whitespace.
        assert_eq!(parse_depends_on(" , db:service_started:true , ,").len(), 1);
        assert!(parse_depends_on("").is_empty());
        assert!(parse_depends_on(",,").is_empty());
    }

    #[test]
    fn an_unknown_condition_degrades_to_started() {
        assert_eq!(
            parse_depends_on("db:service_teleported:true")[0].condition,
            Condition::Started
        );
    }

    #[test]
    fn one_shot_services_are_those_awaited_for_completion() {
        let members = [
            member(
                "stack-web-1",
                "web",
                "migrate:service_completed_successfully:false",
            ),
            member("stack-migrate-1", "migrate", ""),
            member("stack-db-1", "db", ""),
        ];
        let awaited = services_awaited_for_completion(&graph_of(&members));
        assert_eq!(awaited, HashSet::from(["migrate".to_owned()]));
    }

    #[test]
    fn topological_order_puts_a_dependency_before_its_dependent() {
        let members = [
            member(
                "stack-web-1",
                "web",
                "migrate:service_completed_successfully:false,db:service_healthy:true",
            ),
            member("stack-migrate-1", "migrate", "db:service_healthy:false"),
            member("stack-db-1", "db", ""),
        ];
        let graph = graph_of(&members);
        let nodes = vec!["web".to_owned(), "migrate".to_owned(), "db".to_owned()];
        assert_eq!(
            topological_order(&nodes, &graph),
            vec!["db", "migrate", "web"]
        );
    }

    #[test]
    fn ordering_a_subset_ignores_edges_that_leave_it() {
        let members = [
            member(
                "stack-web-1",
                "web",
                "migrate:service_completed_successfully:false,db:service_healthy:true",
            ),
            member("stack-migrate-1", "migrate", "db:service_healthy:false"),
            member("stack-db-1", "db", ""),
        ];
        let graph = graph_of(&members);
        // `db` is on a different image, so it is not part of this rollout; the
        // migrate → web edge still has to hold.
        let nodes = vec!["web".to_owned(), "migrate".to_owned()];
        assert_eq!(topological_order(&nodes, &graph), vec!["migrate", "web"]);
    }

    #[test]
    fn independent_services_are_ordered_by_name_so_a_plan_is_reproducible() {
        let members = [
            member("stack-c-1", "c", ""),
            member("stack-a-1", "a", ""),
            member("stack-b-1", "b", ""),
        ];
        let graph = graph_of(&members);
        let nodes = vec!["c".to_owned(), "a".to_owned(), "b".to_owned()];
        assert_eq!(topological_order(&nodes, &graph), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_dependency_cycle_falls_back_to_name_order_instead_of_hanging() {
        let members = [
            member("stack-a-1", "a", "b:service_started:false"),
            member("stack-b-1", "b", "a:service_started:false"),
            member("stack-z-1", "z", ""),
        ];
        let graph = graph_of(&members);
        let nodes = vec!["a".to_owned(), "b".to_owned(), "z".to_owned()];
        let order = topological_order(&nodes, &graph);
        // `z` is orderable and comes out first; the cycle is drained after it.
        assert_eq!(order, vec!["z", "a", "b"]);
    }

    #[test]
    fn a_service_depending_on_itself_does_not_deadlock_the_sort() {
        let members = [member("stack-a-1", "a", "a:service_started:false")];
        let graph = graph_of(&members);
        assert_eq!(topological_order(&["a".to_owned()], &graph), vec!["a"]);
    }

    #[test]
    fn restart_dependents_only_follow_an_explicit_restart_true_edge() {
        let members = [
            member("stack-web-1", "web", "db:service_healthy:true"),
            member("stack-worker-1", "worker", "db:service_healthy:false"),
            member("stack-db-1", "db", ""),
        ];
        let graph = graph_of(&members);
        let updated = HashSet::from(["db".to_owned()]);
        assert_eq!(restart_dependents(&graph, &updated), vec!["web"]);
    }

    #[test]
    fn a_service_that_was_itself_updated_is_not_also_restarted() {
        let members = [
            member("stack-web-1", "web", "db:service_healthy:true"),
            member("stack-db-1", "db", ""),
        ];
        let graph = graph_of(&members);
        let updated = HashSet::from(["db".to_owned(), "web".to_owned()]);
        assert!(restart_dependents(&graph, &updated).is_empty());
    }

    #[test]
    fn a_duplicate_service_entry_is_collapsed() {
        let deps = parse_depends_on("db:service_healthy:true,db:service_started:false");
        assert_eq!(deps.len(), 1);
    }
}
