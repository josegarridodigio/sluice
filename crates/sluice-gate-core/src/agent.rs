//! Detection of coding-agent callers (ADR-0004).

use std::collections::BTreeMap;

/// Who invoked Sluice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A coding agent; `rule` names the detection rule that fired.
    Agent {
        /// Human-readable description of the rule.
        rule: String,
    },
    /// A person (or anything not recognised as an agent).
    Human,
}

impl Caller {
    /// Whether the caller is a coding agent.
    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Decide whether the caller is an agent.
///
/// Order: `SLUICE_AGENT` (truthy forces agent, falsy forces human, other
/// values are ignored), then any variable of `env_any_set` that is set and
/// non-empty, then any exact `key=value` match of `env_any_equals`.
pub fn detect(
    env: &BTreeMap<String, String>,
    env_any_set: &[String],
    env_any_equals: &BTreeMap<String, String>,
) -> Caller {
    match env.get("SLUICE_AGENT").and_then(|v| parse_bool(v)) {
        Some(true) => {
            return Caller::Agent {
                rule: "SLUICE_AGENT=1".to_owned(),
            };
        }
        Some(false) => return Caller::Human,
        None => {}
    }
    for name in env_any_set {
        if env.get(name).is_some_and(|v| !v.is_empty()) {
            return Caller::Agent {
                rule: format!("{name} is set"),
            };
        }
    }
    for (k, v) in env_any_equals {
        if env.get(k) == Some(v) {
            return Caller::Agent {
                rule: format!("{k}={v}"),
            };
        }
    }
    Caller::Human
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn defaults() -> Vec<String> {
        vec!["CLAUDECODE".to_owned()]
    }

    #[test]
    fn empty_env_is_human() {
        let c = detect(&env(&[]), &defaults(), &BTreeMap::new());
        assert_eq!(c, Caller::Human);
        assert!(!c.is_agent());
    }

    #[test]
    fn sluice_agent_truthy_forces_agent() {
        for v in ["1", "true", "YES", "On"] {
            let c = detect(&env(&[("SLUICE_AGENT", v)]), &[], &BTreeMap::new());
            assert_eq!(
                c,
                Caller::Agent {
                    rule: "SLUICE_AGENT=1".into()
                }
            );
            assert!(c.is_agent());
        }
    }

    #[test]
    fn sluice_agent_falsy_overrides_everything() {
        let eq = env(&[("X", "y")]);
        for v in ["0", "false", "No", "OFF"] {
            let e = env(&[("SLUICE_AGENT", v), ("CLAUDECODE", "1"), ("X", "y")]);
            assert_eq!(detect(&e, &defaults(), &eq), Caller::Human);
        }
    }

    #[test]
    fn unknown_sluice_agent_value_falls_through() {
        let e = env(&[("SLUICE_AGENT", "maybe"), ("CLAUDECODE", "1")]);
        assert_eq!(
            detect(&e, &defaults(), &BTreeMap::new()),
            Caller::Agent {
                rule: "CLAUDECODE is set".into()
            }
        );
        let e = env(&[("SLUICE_AGENT", "maybe")]);
        assert_eq!(detect(&e, &defaults(), &BTreeMap::new()), Caller::Human);
    }

    #[test]
    fn env_any_set_requires_non_empty() {
        assert_eq!(
            detect(&env(&[("CLAUDECODE", "")]), &defaults(), &BTreeMap::new()),
            Caller::Human
        );
        assert!(detect(&env(&[("CLAUDECODE", "1")]), &defaults(), &BTreeMap::new()).is_agent());
    }

    #[test]
    fn env_any_equals_matches_exactly() {
        let eq = env(&[("TERM_PROGRAM", "agent")]);
        assert_eq!(
            detect(&env(&[("TERM_PROGRAM", "agent")]), &[], &eq),
            Caller::Agent {
                rule: "TERM_PROGRAM=agent".into()
            }
        );
        assert_eq!(
            detect(&env(&[("TERM_PROGRAM", "agent2")]), &[], &eq),
            Caller::Human
        );
    }

    #[test]
    fn env_any_set_takes_precedence_over_equals() {
        let eq = env(&[("A", "b")]);
        let e = env(&[("CLAUDECODE", "1"), ("A", "b")]);
        assert_eq!(
            detect(&e, &defaults(), &eq),
            Caller::Agent {
                rule: "CLAUDECODE is set".into()
            }
        );
    }
}
