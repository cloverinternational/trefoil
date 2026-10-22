//! `routing/service-tier-suffix.ts`: `<alias>@<tier>` model-name suffix.
//! Purely syntactic; whether the bare name is an alias is decided by
//! `router::split_service_tier`.

pub const SERVICE_TIER_SUFFIX_DELIMITER: char = '@';

/// `SERVICE_TIER_SUFFIXES`.
pub const SERVICE_TIER_SUFFIXES: [&str; 7] = [
    "auto",
    "default",
    "standard",
    "flex",
    "priority",
    "fast",
    "ultrafast",
];

/// `ServiceTierSplit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTierSplit {
    /// The model name with any recognised tier suffix removed.
    pub model: String,
    /// The normalised (lower-case) tier, when one was split off.
    pub service_tier: Option<String>,
}

impl ServiceTierSplit {
    pub fn unchanged(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            service_tier: None,
        }
    }
}

/// `splitServiceTierSuffix`: splits on the LAST delimiter, only when a
/// non-empty model precedes it and the text after it is a known tier.
pub fn split_service_tier_suffix(model_name: &str) -> ServiceTierSplit {
    let Some(index) = model_name.rfind(SERVICE_TIER_SUFFIX_DELIMITER) else {
        return ServiceTierSplit::unchanged(model_name);
    };
    if index == 0 {
        return ServiceTierSplit::unchanged(model_name);
    }
    let tier = model_name[index + 1..].to_lowercase();
    if !SERVICE_TIER_SUFFIXES.contains(&tier.as_str()) {
        return ServiceTierSplit::unchanged(model_name);
    }
    ServiceTierSplit {
        model: model_name[..index].to_owned(),
        service_tier: Some(tier),
    }
}

/// `serviceTierNames`: every `<model>@<tier>` spelling naming the same tier.
pub fn service_tier_names(model: &str, tier: &str) -> Vec<String> {
    let names: Vec<&str> = match tier {
        "priority" | "fast" => vec!["priority", "fast"],
        "default" | "standard" => vec!["default", "standard"],
        other => vec![other],
    };
    names
        .into_iter()
        .map(|n| format!("{model}{SERVICE_TIER_SUFFIX_DELIMITER}{n}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_known_tiers() {
        for tier in SERVICE_TIER_SUFFIXES {
            assert_eq!(
                split_service_tier_suffix(&format!("gpt-6-luna@{tier}")),
                ServiceTierSplit {
                    model: "gpt-6-luna".into(),
                    service_tier: Some(tier.into())
                }
            );
        }
        assert_eq!(
            split_service_tier_suffix("gpt-6-luna@FLEX")
                .service_tier
                .as_deref(),
            Some("flex")
        );
        assert_eq!(
            split_service_tier_suffix("vendor@2024@flex"),
            ServiceTierSplit {
                model: "vendor@2024".into(),
                service_tier: Some("flex".into())
            }
        );
    }

    #[test]
    fn leaves_other_names_unchanged() {
        for name in [
            "gpt-6-luna",
            "gpt-6-luna@turbo",
            "gpt-6-luna@",
            "@flex",
            "gpt-6-luna@flex:high",
            "gpt-6-luna@preset/flex",
            "gpt-6-luna@flex ",
        ] {
            assert_eq!(
                split_service_tier_suffix(name),
                ServiceTierSplit::unchanged(name),
                "{name}"
            );
        }
    }

    #[test]
    fn tier_names() {
        assert_eq!(service_tier_names("m", "flex"), vec!["m@flex"]);
        assert_eq!(
            service_tier_names("m", "fast"),
            vec!["m@priority", "m@fast"]
        );
        assert_eq!(
            service_tier_names("m", "default"),
            vec!["m@default", "m@standard"]
        );
        assert_eq!(
            service_tier_names("m", "standard"),
            vec!["m@default", "m@standard"]
        );
    }
}
