//! Runner classes: the resources a job of each size needs.
//!
//! Jobs ask for a class through its runner label (`<prefix><class>` for main
//! and merge-group jobs, `<prefix><class>-pr` for pull requests,
//! `<prefix><class>-deploy` for deploy-path jobs). The prefix
//! and the per-class requirements are configuration with generic defaults.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Default label prefix shared by every class label.
pub const DEFAULT_LABEL_PREFIX: &str = "borg-ci-";
/// Most extra labels one drone may add to its runners.
pub const MAX_EXTRA_LABELS: usize = 16;
/// Suffix marking pull-request demand.
const PR_SUFFIX: &str = "-pr";
/// Suffix marking deploy-path demand.
const DEPLOY_SUFFIX: &str = "-deploy";

/// A runner size class. Ordered smallest to largest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Light,
    Medium,
    Heavy,
}

impl Class {
    /// Every class, smallest first.
    pub const ALL: [Class; 3] = [Class::Light, Class::Medium, Class::Heavy];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Light => "light",
            Class::Medium => "medium",
            Class::Heavy => "heavy",
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a queued job comes from. Deploy jobs (prod OIDC, signing, registry
/// push) are scheduled before main jobs (main branch, merge groups), which are
/// scheduled before pull-request jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Deploy,
    Main,
    Pr,
}

impl Scope {
    /// Every scope in scheduling order.
    pub const ALL: [Scope; 3] = [Scope::Deploy, Scope::Main, Scope::Pr];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Deploy => "deploy",
            Scope::Main => "main",
            Scope::Pr => "pr",
        }
    }
}

/// How much a drone's host is trusted with deploy-path jobs. A deploy drone
/// never runs a pull-request job or a work unit; a shared drone never runs a
/// deploy job; a deploy-only drone runs nothing but deploy jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    #[default]
    Shared,
    Deploy,
    DeployOnly,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Shared => "shared",
            Trust::Deploy => "deploy",
            Trust::DeployOnly => "deploy-only",
        }
    }

    /// Whether demand of `scope` may be placed on a drone of this trust.
    pub fn admits(self, scope: Scope) -> bool {
        match scope {
            Scope::Deploy => self != Trust::Shared,
            Scope::Main => self != Trust::DeployOnly,
            Scope::Pr => self == Trust::Shared,
        }
    }

    /// Whether drones of this trust hold standby deploy runners.
    pub fn serves_deploy(self) -> bool {
        self != Trust::Shared
    }
}

/// What an alias label stands for: a bare class name (main scope) or an object
/// naming both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Alias {
    Main(Class),
    Scoped(AliasTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasTarget {
    pub class: Class,
    pub scope: Scope,
}

impl Alias {
    pub fn target(self) -> (Class, Scope) {
        match self {
            Alias::Main(class) => (class, Scope::Main),
            Alias::Scoped(t) => (t.class, t.scope),
        }
    }
}

/// Resources reserved for one running job of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassSpec {
    pub mem_mib: u64,
    pub vcpu: u32,
    /// The host must have a usable Docker daemon.
    pub docker: bool,
}

/// Requirements for all classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Classes {
    /// Prefix of every class label, e.g. `example-ci-` gives `example-ci-light`.
    pub label_prefix: String,
    pub light: ClassSpec,
    pub medium: ClassSpec,
    pub heavy: ClassSpec,
    /// Existing runner labels that borg also answers, each mapped to the class
    /// that serves it (for example a deploy lane's `runs-on` label). A plain
    /// class name (`"medium"`) is main demand; `{"class": "medium", "scope":
    /// "deploy"}` is deploy demand. Queued jobs with an alias label count as
    /// demand of its scope, and a runner carries the alias label only when its
    /// grant scope equals the alias's (main aliases also ride on PR runners,
    /// which carry the bare label).
    pub aliases: BTreeMap<String, Alias>,
}

impl Default for Classes {
    fn default() -> Self {
        Self {
            label_prefix: DEFAULT_LABEL_PREFIX.to_owned(),
            aliases: BTreeMap::new(),
            // 2.5 GiB p90, 2 vCPU.
            light: ClassSpec {
                mem_mib: 2560,
                vcpu: 2,
                docker: false,
            },
            // 12 GiB, Docker.
            medium: ClassSpec {
                mem_mib: 12 * 1024,
                vcpu: 4,
                docker: true,
            },
            // 24 GiB, Docker.
            heavy: ClassSpec {
                mem_mib: 24 * 1024,
                vcpu: 8,
                docker: true,
            },
        }
    }
}

/// Invalid class configuration.
#[derive(Debug, thiserror::Error)]
pub enum ClassesError {
    #[error("parse classes config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("label prefix must not be empty")]
    EmptyPrefix,
    #[error(
        "alias label {0:?} is malformed, starts with the class label prefix, or has scope \"pr\""
    )]
    BadAlias(String),
    #[error("class {class} must reserve at least 1 MiB and 1 vCPU")]
    Empty { class: Class },
    #[error("class {larger} must not reserve less than {smaller}")]
    NotMonotonic { smaller: Class, larger: Class },
}

impl Classes {
    pub fn spec(&self, class: Class) -> &ClassSpec {
        match class {
            Class::Light => &self.light,
            Class::Medium => &self.medium,
            Class::Heavy => &self.heavy,
        }
    }

    /// Parse a JSON config. Omitted classes keep their defaults.
    pub fn from_json(json: &str) -> Result<Self, ClassesError> {
        let classes: Self = serde_json::from_str(json)?;
        classes.validate()?;
        Ok(classes)
    }

    /// Reject configs the allocator cannot reason about: zero-sized classes
    /// and classes that shrink as they grow.
    pub fn validate(&self) -> Result<(), ClassesError> {
        if self.label_prefix.is_empty() {
            return Err(ClassesError::EmptyPrefix);
        }
        for (label, alias) in &self.aliases {
            if !valid_extra_label(label)
                || label.starts_with(self.label_prefix.as_str())
                || alias.target().1 == Scope::Pr
            {
                return Err(ClassesError::BadAlias(label.clone()));
            }
        }
        for class in Class::ALL {
            let spec = self.spec(class);
            if spec.mem_mib == 0 || spec.vcpu == 0 {
                return Err(ClassesError::Empty { class });
            }
        }
        for pair in Class::ALL.windows(2) {
            let (smaller, larger) = (pair[0], pair[1]);
            let (s, l) = (self.spec(smaller), self.spec(larger));
            if l.mem_mib < s.mem_mib || l.vcpu < s.vcpu || (s.docker && !l.docker) {
                return Err(ClassesError::NotMonotonic { smaller, larger });
            }
        }
        Ok(())
    }

    /// Keep the extra runner labels a drone asked for that are safe to add:
    /// well-formed, not `self-hosted`, not a label of any class (which could
    /// change which jobs the runner is offered), no duplicates, at most 16.
    pub fn sanitize_extra_labels(&self, labels: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for label in labels {
            if valid_extra_label(label) && self.parse_label(label).is_none() && !out.contains(label)
            {
                out.push(label.clone());
            }
        }
        out.truncate(MAX_EXTRA_LABELS);
        out
    }

    /// The class and scope a runner minted with `labels` will serve: pull
    /// request runners carry the `-pr` label, main-only runners only the bare
    /// one. Labels that are not class labels never affect the answer.
    pub fn labels_scope(&self, labels: &[String]) -> Option<(Class, Scope)> {
        let mut found: Option<(Class, Scope)> = None;
        for (class, scope) in labels.iter().filter_map(|l| self.parse_label(l)) {
            found = match found {
                Some((c, Scope::Pr)) if c == class => Some((c, Scope::Pr)),
                _ => Some((class, scope)),
            };
        }
        found
    }

    /// The label a job requests for `class` and `scope`.
    pub fn job_label(&self, class: Class, scope: Scope) -> String {
        match scope {
            Scope::Main => format!("{}{class}", self.label_prefix),
            Scope::Pr => format!("{}{class}{PR_SUFFIX}", self.label_prefix),
            Scope::Deploy => format!("{}{class}{DEPLOY_SUFFIX}", self.label_prefix),
        }
    }

    /// Parse a job's `runs-on` label into its class and scope.
    pub fn parse_label(&self, label: &str) -> Option<(Class, Scope)> {
        if let Some(alias) = self.aliases.get(label) {
            return Some(alias.target());
        }
        let rest = label.strip_prefix(self.label_prefix.as_str())?;
        let (name, scope) = if let Some(name) = rest.strip_suffix(DEPLOY_SUFFIX) {
            (name, Scope::Deploy)
        } else if let Some(name) = rest.strip_suffix(PR_SUFFIX) {
            (name, Scope::Pr)
        } else {
            (rest, Scope::Main)
        };
        let class = Class::ALL.into_iter().find(|c| c.as_str() == name)?;
        Some((class, scope))
    }

    /// The exact label set a runner is minted with.
    ///
    /// Main grants carry only the bare label so GitHub can hand them nothing
    /// but main jobs. Pull-request grants carry both labels. Deploy grants
    /// carry only the `-deploy` label, so GitHub can hand them nothing but
    /// deploy jobs. Alias labels ride on the runners whose scope they name.
    pub fn runner_labels(&self, class: Class, scope: Scope) -> Vec<String> {
        let mut labels = match scope {
            Scope::Deploy => vec![self.job_label(class, Scope::Deploy)],
            Scope::Main => vec![self.job_label(class, Scope::Main)],
            Scope::Pr => vec![
                self.job_label(class, Scope::Main),
                self.job_label(class, Scope::Pr),
            ],
        };
        labels.extend(
            self.aliases
                .iter()
                .filter(|&(_, a)| {
                    let (c, s) = a.target();
                    c == class && (s == scope || (s == Scope::Main && scope == Scope::Pr))
                })
                .map(|(l, _)| l.clone()),
        );
        labels
    }
}

/// A label the drone may add to its runners: 1 to 64 characters of letters,
/// digits, `.`, `_` and `-`, and not the reserved `self-hosted`.
pub fn valid_extra_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !label.eq_ignore_ascii_case("self-hosted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design_and_validate() {
        let c = Classes::default();
        assert_eq!(c.light.mem_mib, 2560);
        assert_eq!(c.light.vcpu, 2);
        assert!(!c.light.docker);
        assert_eq!(c.medium.mem_mib, 12 * 1024);
        assert!(c.medium.docker);
        assert_eq!(c.heavy.mem_mib, 24 * 1024);
        assert!(c.heavy.docker);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn labels_round_trip() {
        let c = Classes::default();
        for class in Class::ALL {
            for scope in Scope::ALL {
                assert_eq!(
                    c.parse_label(&c.job_label(class, scope)),
                    Some((class, scope))
                );
            }
        }
        assert_eq!(c.job_label(Class::Heavy, Scope::Pr), "borg-ci-heavy-pr");
        assert_eq!(c.job_label(Class::Light, Scope::Main), "borg-ci-light");
    }

    #[test]
    fn prefix_is_configurable() {
        let c = Classes::from_json(r#"{"label_prefix":"example-ci-"}"#).unwrap();
        assert_eq!(
            c.job_label(Class::Medium, Scope::Pr),
            "example-ci-medium-pr"
        );
        assert_eq!(
            c.parse_label("example-ci-medium"),
            Some((Class::Medium, Scope::Main))
        );
        assert_eq!(c.parse_label("borg-ci-medium"), None);
        assert!(matches!(
            Classes::from_json(r#"{"label_prefix":""}"#),
            Err(ClassesError::EmptyPrefix)
        ));
    }

    #[test]
    fn unknown_labels_do_not_parse() {
        let c = Classes::default();
        for label in [
            "ubuntu-latest",
            "borg-ci-",
            "borg-ci-huge",
            "borg-ci-light-pr-pr",
            "borg-ci-pr",
        ] {
            assert_eq!(c.parse_label(label), None, "{label}");
        }
    }

    #[test]
    fn main_runner_has_only_the_bare_label() {
        assert_eq!(
            Classes::default().runner_labels(Class::Medium, Scope::Main),
            ["borg-ci-medium"]
        );
    }

    #[test]
    fn pr_runner_has_both_labels() {
        assert_eq!(
            Classes::default().runner_labels(Class::Medium, Scope::Pr),
            ["borg-ci-medium", "borg-ci-medium-pr"]
        );
    }

    #[test]
    fn json_overrides_one_class_and_keeps_defaults() {
        let c =
            Classes::from_json(r#"{"heavy":{"mem_mib":20480,"vcpu":6,"docker":true}}"#).unwrap();
        assert_eq!(c.heavy.mem_mib, 20480);
        assert_eq!(c.light, Classes::default().light);
    }

    #[test]
    fn json_rejects_unknown_fields_and_bad_shapes() {
        assert!(matches!(
            Classes::from_json(r#"{"huge":{}}"#),
            Err(ClassesError::Parse(_))
        ));
        assert!(matches!(
            Classes::from_json(r#"{"light":{"mem_mib":0,"vcpu":2,"docker":false}}"#),
            Err(ClassesError::Empty {
                class: Class::Light
            })
        ));
        assert!(matches!(
            Classes::from_json(r#"{"heavy":{"mem_mib":1024,"vcpu":8,"docker":true}}"#),
            Err(ClassesError::NotMonotonic {
                smaller: Class::Medium,
                larger: Class::Heavy
            })
        ));
    }

    #[test]
    fn extra_labels_are_filtered_and_never_change_the_scope() {
        let c = Classes::default();
        let extras: Vec<String> = [
            "legacy-lane",
            "on-prem",
            "borg-ci-heavy-pr",
            "borg-ci-light",
            "self-hosted",
            "bad label",
            "on-prem",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            c.sanitize_extra_labels(&extras),
            ["legacy-lane", "on-prem"],
            "class labels, duplicates and malformed labels are dropped"
        );

        for (class, scope) in [
            (Class::Medium, Scope::Pr),
            (Class::Medium, Scope::Main),
            (Class::Light, Scope::Pr),
        ] {
            let mut labels = c.runner_labels(class, scope);
            labels.extend(c.sanitize_extra_labels(&extras));
            assert_eq!(
                c.labels_scope(&labels),
                Some((class, scope)),
                "{class} {scope:?}"
            );
        }
        // Without the sanitizer a smuggled class label would flip a PR runner to the main lane.
        let mut smuggled = c.runner_labels(Class::Medium, Scope::Main);
        smuggled.push("borg-ci-medium-pr".to_owned());
        assert_eq!(c.labels_scope(&smuggled), Some((Class::Medium, Scope::Pr)));
        assert_eq!(
            c.sanitize_extra_labels(&["borg-ci-medium-pr".to_owned()]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn aliases_are_main_demand_and_ride_on_class_runners() {
        let c = Classes::from_json(
            r#"{"label_prefix":"x-ci-","aliases":{"deploy-lane":"medium","publisher":"heavy"}}"#,
        )
        .unwrap();
        assert_eq!(
            c.parse_label("deploy-lane"),
            Some((Class::Medium, Scope::Main))
        );
        assert_eq!(
            c.parse_label("publisher"),
            Some((Class::Heavy, Scope::Main))
        );
        assert_eq!(
            c.runner_labels(Class::Medium, Scope::Pr),
            ["x-ci-medium", "x-ci-medium-pr", "deploy-lane"]
        );
        assert_eq!(c.runner_labels(Class::Light, Scope::Main), ["x-ci-light"]);
        assert_eq!(
            c.labels_scope(&c.runner_labels(Class::Medium, Scope::Pr)),
            Some((Class::Medium, Scope::Pr))
        );
        assert!(
            c.sanitize_extra_labels(&["deploy-lane".to_owned()])
                .is_empty(),
            "an alias cannot be smuggled in as an extra label"
        );
        for bad in [
            r#"{"aliases":{"borg-ci-x":"light"}}"#,
            r#"{"aliases":{"a b":"light"}}"#,
        ] {
            assert!(
                matches!(Classes::from_json(bad), Err(ClassesError::BadAlias(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn deploy_labels_round_trip_and_deploy_runners_carry_only_their_label() {
        let c = Classes::default();
        assert_eq!(
            c.job_label(Class::Medium, Scope::Deploy),
            "borg-ci-medium-deploy"
        );
        assert_eq!(
            c.parse_label("borg-ci-heavy-deploy"),
            Some((Class::Heavy, Scope::Deploy))
        );
        assert_eq!(c.parse_label("borg-ci-heavy-deploy-pr"), None);
        assert_eq!(
            c.runner_labels(Class::Medium, Scope::Deploy),
            ["borg-ci-medium-deploy"]
        );
        assert_eq!(
            c.labels_scope(&c.runner_labels(Class::Light, Scope::Deploy)),
            Some((Class::Light, Scope::Deploy))
        );
        assert!(Scope::Deploy < Scope::Main && Scope::Main < Scope::Pr);
    }

    #[test]
    fn trust_admits_scopes() {
        assert!(Trust::Deploy.admits(Scope::Deploy) && Trust::Deploy.admits(Scope::Main));
        assert!(!Trust::Deploy.admits(Scope::Pr));
        assert!(Trust::Shared.admits(Scope::Main) && Trust::Shared.admits(Scope::Pr));
        assert!(!Trust::Shared.admits(Scope::Deploy));
        assert!(Trust::DeployOnly.admits(Scope::Deploy));
        assert!(!Trust::DeployOnly.admits(Scope::Main));
        assert!(!Trust::DeployOnly.admits(Scope::Pr));
    }

    #[test]
    fn deploy_only_trust_serializes_kebab_case() {
        assert_eq!(Trust::DeployOnly.as_str(), "deploy-only");
        let t: Trust = serde_json::from_str("\"deploy-only\"").unwrap();
        assert_eq!(t, Trust::DeployOnly);
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"deploy-only\"");
        assert!(Trust::Deploy.serves_deploy() && !Trust::Shared.serves_deploy());
    }

    #[test]
    fn scoped_aliases_ride_only_on_runners_of_their_scope() {
        let c = Classes::from_json(
            r#"{"label_prefix":"x-ci-","aliases":{
                "promote":{"class":"medium","scope":"deploy"},
                "legacy":"medium"}}"#,
        )
        .unwrap();
        assert_eq!(
            c.parse_label("promote"),
            Some((Class::Medium, Scope::Deploy))
        );
        assert_eq!(c.parse_label("legacy"), Some((Class::Medium, Scope::Main)));
        assert_eq!(
            c.runner_labels(Class::Medium, Scope::Deploy),
            ["x-ci-medium-deploy", "promote"]
        );
        assert_eq!(
            c.runner_labels(Class::Medium, Scope::Main),
            ["x-ci-medium", "legacy"]
        );
        assert_eq!(
            c.runner_labels(Class::Medium, Scope::Pr),
            ["x-ci-medium", "x-ci-medium-pr", "legacy"]
        );
        assert_eq!(
            c.runner_labels(Class::Heavy, Scope::Deploy),
            ["x-ci-heavy-deploy"]
        );
    }

    #[test]
    fn alias_labels_must_not_look_like_class_labels() {
        for bad in [
            r#"{"aliases":{"borg-ci-medium-deploy":"light"}}"#,
            r#"{"aliases":{"borg-ci-medium":{"class":"light","scope":"deploy"}}}"#,
            r#"{"aliases":{"lane":{"class":"light","scope":"pr"}}}"#,
        ] {
            assert!(
                matches!(Classes::from_json(bad), Err(ClassesError::BadAlias(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            Classes::from_json(r#"{"aliases":{"lane":{"class":"light","scope":"deploy","x":1}}}"#),
            Err(ClassesError::Parse(_))
        ));
    }

    #[test]
    fn extra_label_syntax() {
        assert!(valid_extra_label("on-prem") && valid_extra_label("a.b_c-1"));
        for bad in [
            "",
            "self-hosted",
            "Self-Hosted",
            "a b",
            "a/b",
            &"x".repeat(65),
        ] {
            assert!(!valid_extra_label(bad), "{bad}");
        }
        let many: Vec<String> = (0..40).map(|i| format!("l{i}")).collect();
        assert_eq!(
            Classes::default().sanitize_extra_labels(&many).len(),
            MAX_EXTRA_LABELS
        );
    }
}
