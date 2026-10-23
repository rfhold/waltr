use std::collections::BTreeMap;

use serde_json::Value;

pub(super) type Results = BTreeMap<String, BTreeMap<String, Value>>;

#[derive(Clone, Debug)]
pub(super) enum TaskOutcome {
    Succeeded(BTreeMap<String, Value>),
    Skipped,
    Failed,
    Interrupted,
}

pub(super) type Outcomes = BTreeMap<String, TaskOutcome>;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DependencyReadiness {
    Waiting,
    Ready,
    Blocked,
}

pub(super) fn dependency_readiness<'a>(
    dependencies: impl IntoIterator<Item = &'a String>,
    outcomes: &Outcomes,
) -> DependencyReadiness {
    let mut waiting = false;
    for name in dependencies {
        match outcomes.get(name) {
            Some(TaskOutcome::Succeeded(_) | TaskOutcome::Skipped) => {}
            Some(TaskOutcome::Failed | TaskOutcome::Interrupted) => {
                return DependencyReadiness::Blocked;
            }
            None => waiting = true,
        }
    }
    if waiting {
        DependencyReadiness::Waiting
    } else {
        DependencyReadiness::Ready
    }
}

pub(super) fn successful_results(outcomes: &Outcomes) -> Results {
    outcomes
        .iter()
        .filter_map(|(name, outcome)| match outcome {
            TaskOutcome::Succeeded(values) => Some((name.clone(), values.clone())),
            TaskOutcome::Skipped => Some((name.clone(), BTreeMap::new())),
            TaskOutcome::Failed | TaskOutcome::Interrupted => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_requires_success_or_skip_and_failure_wins_over_waiting() {
        let dependency = "before".to_owned();
        let missing = "missing".to_owned();
        for outcome in [
            TaskOutcome::Succeeded(BTreeMap::new()),
            TaskOutcome::Skipped,
        ] {
            let states = BTreeMap::from([(dependency.clone(), outcome)]);
            assert_eq!(
                dependency_readiness([&dependency], &states),
                DependencyReadiness::Ready
            );
        }
        for outcome in [TaskOutcome::Failed, TaskOutcome::Interrupted] {
            let states = BTreeMap::from([(dependency.clone(), outcome)]);
            assert_eq!(
                dependency_readiness([&missing, &dependency], &states),
                DependencyReadiness::Blocked
            );
            assert!(successful_results(&states).is_empty());
        }
        assert_eq!(
            dependency_readiness([&dependency], &Outcomes::new()),
            DependencyReadiness::Waiting
        );
    }
}
