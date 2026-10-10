//! Runner classes: the resources a job of each size needs.
//!
//! Jobs ask for a class through its runner label (`<prefix><class>` for main
//! and merge-group jobs, `<prefix><class>-pr` for pull requests). The prefix
//! and the per-class requirements are configuration with generic defaults.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Default label prefix shared by every class label.
pub const DEFAULT_LABEL_PREFIX: &str = "borg-ci-";
/// Suffix marking pull-request demand.
const PR_SUFFIX: &str = "-pr";

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

/// Where a queued job comes from. Main jobs (main branch, merge groups) are
/// scheduled before pull-request jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Main,
    Pr,
}

impl Scope {
    pub const ALL: [Scope; 2] = [Scope::Main, Scope::Pr];
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
}

impl Default for Classes {
    fn default() -> Self {
        Self {
            label_prefix: DEFAULT_LABEL_PREFIX.to_owned(),
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

    /// The label a job requests for `class` and `scope`.
    pub fn job_label(&self, class: Class, scope: Scope) -> String {
        match scope {
            Scope::Main => format!("{}{class}", self.label_prefix),
            Scope::Pr => format!("{}{class}{PR_SUFFIX}", self.label_prefix),
        }
    }

    /// Parse a job's `runs-on` label into its class and scope.
    pub fn parse_label(&self, label: &str) -> Option<(Class, Scope)> {
        let rest = label.strip_prefix(self.label_prefix.as_str())?;
        let (name, scope) = match rest.strip_suffix(PR_SUFFIX) {
            Some(name) => (name, Scope::Pr),
            None => (rest, Scope::Main),
        };
        let class = Class::ALL.into_iter().find(|c| c.as_str() == name)?;
        Some((class, scope))
    }

    /// The exact label set a runner is minted with.
    ///
    /// Main grants carry only the bare label so GitHub can hand them nothing
    /// but main jobs. Pull-request grants carry both labels.
    pub fn runner_labels(&self, class: Class, scope: Scope) -> Vec<String> {
        match scope {
            Scope::Main => vec![self.job_label(class, Scope::Main)],
            Scope::Pr => vec![
                self.job_label(class, Scope::Main),
                self.job_label(class, Scope::Pr),
            ],
        }
    }
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
}
